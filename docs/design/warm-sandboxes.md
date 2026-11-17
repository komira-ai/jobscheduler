# Warm sandboxes: containers made ahead of their jobs

Status: **built** (`crates/js-agent/src/crun/warm.rs`, `crates/js-contain/src/oci/held.rs`,
`crates/js-contain/src/job_fs/inject.rs`, the `--wait-job` mode of `crates/js-contain/init/init.rs`).

**What changes for a job.** Before: a job start on the `crun` runtime makes its container from nothing while the
job waits. The agent writes the bundle, makes the lease's directories and cgroup, and starts a supervisor. The
supervisor then runs `crun run`, which makes the namespaces, the user mapping, the mounts and `pivot_root`, and
execs the init, which starts the program. After: for an image that started on the node in the last ten minutes,
the agent keeps up to `--warm-sandboxes` (default 2) containers made and started ahead of any job. Each is up to
its init, which waits on a pipe. A job start of that image takes one. The agent binds the job's directories into
the running container, sets the lease's limits to the job's booking, and writes the program, its arguments, its
directory and its environment to the pipe. The init starts the program at once. What the job sees does not
change: the same root, `/work`, `/scratch`, mounts, run files, home, environment, user, limits and isolation as
a cold start. What does not change at all:

- a sandbox is never reused: the container is removed with its run, as a cold one is;
- every job a sandbox cannot carry is made cold, exactly as before;
- every failure before the job is handed over removes the sandbox and makes the job cold.

**The goal.** The goal is a time from assignment to exec under 50 ms p50 for a job of a common image. The
agent's own log of each start now gives its steps in milliseconds after the reply that carried it was received
(`Event::Started { steps: StartSteps { admitted, ready, dirs, create, started, warm } }`). That line is the
measurement, on a farm node, of what a warm start saves over a cold one. Nothing here claims a number before that
measurement.

## 1. Which runs a sandbox carries

A sandbox is the container a cold start would make, apart from what a bind or a write can change afterwards. It
carries a run that is:

- contained;
- of an image;
- without a network (`none`: loopback only);
- without a GPU;
- without a port or a host port;
- without a layered root.

Mounts (host paths, provider mounts, copy-on-write mounts), run files, a process limit, live logs and any booking
are carried. Every other run is made cold. The pool runs only where the job file system runs (the kernel's new
mount API: `open_tree`, `move_mount`, `mount_setattr`) and where leases are made beside the runs (every node:
`OciConfig::leases_in`).

## 2. The pool

- **Sizing.** An image gets no sandbox until a contained start of it is counted. The sizing counts every start
  the pool could carry by its shape (§1), whether or not it found a sandbox. An image keeps sandboxes for ten
  minutes after its last start (`KEEP_MS`). For that time it has as many as it started in the last minute
  (`RECENT_MS`), at least one and at most `--warm-sandboxes`. `--warm-sandboxes 0` turns the pool off.
- **Tending.** The lifecycle calls `Runtime::tend` once a step, after the step's starts. Tending removes the
  sandboxes that ended and those an image no longer needs (oldest first), then makes those an image lacks. An
  image whose sandbox cannot be made is not tried again for 30 s (`RETRY_MS`).
- **Making a sandbox.** It is made as a run's container is (`make`, held). Its own id is `w<epoch>-<n>`. Its
  bundle is the loopback bundle, with the init's arguments `--wait-job /dev/js-job --` and the lease's named
  pipe `job` (mode `0600`, given to the job's id) bound read-only at `/dev/js-job`. Its `/scratch` is its own
  empty input root, so the mount point exists.
- **Its booking.** Its lease cgroup books 1 CPU and 64 MiB until a run takes it: the init and `crun`'s setup
  need less.
- **Its logs.** Its capture is capped as a live-logs run's would be, since a sandbox does not know whether its
  run will upload.
- **Its sidecar.** The sidecar stays in the state *starting*, with the label `jobscheduler.dev/warm` naming the
  image.
- **Not a run.** Sandboxes are not in the runtime's runs. They are never listed as containers and never adopted.
  `Runtime::idle_handles` names them, and the post-job and taint checks count their processes as live (the
  runtime's own), never as a job's leftovers.

## 3. A start in a sandbox (`CrunRuntime::start_warm`)

The runtime takes an idle sandbox of the run's image that is up: its container's group `ctr` holds one process,
the init. In order:

1. The run's container spec is built as a cold start builds it: the image's kept config with the run's arguments
   and the job's environment over the image's, and the assembly's mounts given.
2. The run's lease directory is taken, as a cold lease's is. The lifecycle made it beside the run's scratch
   directory, under the run's quota. Its `root`, `upper`, `work`, `rw` and `rw/home` are made and given as a cold
   lease's are. The run's scratch directory and files are given to the job's id.
3. The lease cgroup's `memory.max`, `memory.swap.max` and `cpu.max` are set from the booking
   (`LeaseCgroup::set_limits`, the hard policy). The container group's `pids.max` is set to the run's limit.
4. The run's directories are bound into the container (`js_contain::job_fs::bind_into`). The binds are, in order:
   - `/work`: the run lease's `upper`, or the copy-on-write mount the run names there;
   - `/root`: the run lease's home;
   - `/scratch`, then the run's mounts, then `/run/js-files`.

   How a bind is made:
   - each source is opened and cloned (`open_tree(OPEN_TREE_CLONE | AT_RECURSIVE)`) and set `nosuid`, `nodev`,
     private, read-only or `noexec` as asked, in the agent's namespace;
   - then one thread of the agent opens the init's `/proc/<pid>/ns/mnt` and `/proc/<pid>/root`, unshares its
     file-system context, joins the mount namespace and `chroot`s into the container's root;
   - for each bind, it makes the missing mount points as the job's id would (`setfsuid`/`setfsgid`: a tmpfs of
     the container's user namespace has no mapping for the agent's uid 0), then attaches the clone with
     `move_mount`. This is what `systemd`'s bind into a running container does;
   - the thread ends.

   `/work` and the home are bound from the run's lease, not the sandbox's. So a warm run's writes count against
   the run's quota, as a cold run's do. The sandbox's own lease holds only its bundle, pipe and logs.
5. The job is handed to the init (`js_contain::oci::hand_job`). The pipe is opened without waiting: an init that
   is not waiting is a failure, and nothing is written. The init reads the job to its end, enters its directory,
   and starts the program on the job's `PATH` with exactly the job's environment.
6. The sidecar is rewritten with the run's labels and booking, in the state *running*, with `lease_id` naming
   the sandbox's id that the lease's name was made from. From here on the run is adopted after a restart as any
   run is. The run's lease directory is kept by the restart's sweep.

The handle of the run is the sandbox's name. `Runtime::started_warm` says it was warm, and the start's
`StartSteps.warm` reports it.

## 4. Failures

- **A failure before step 5.** This covers a bind refused, a limit that cannot be written, or a lease directory
  that cannot be taken. The runtime removes the sandbox and empties the run's lease directory of what step 2
  made, and the run is made cold.
- **A failure of step 5.** Nothing ran: the init exits when it reads no well-formed job. The runtime removes the
  sandbox, and the run is made cold.
- **A failure of step 6.** The run has started, and the start fails as a cold start whose sidecar cannot be
  written does. The run is removed.
- **A crash before step 6.** The sidecar still says *starting*, so the next start of the agent removes the lease
  as one that never ran. The run is lost, and the lease server runs it again, as for any lost run.
- **A crash after step 6.** The run is adopted.
- **A sandbox that ends while idle.** It is removed at the next step and replaced. An idle sandbox is removed at
  every agent start.
- **Malformed jobs.** A job the init cannot read is the node's failure: exit 126, a line in the init's report.
  This covers a job not in the form `js-job/1`, a relative directory, no program, or counts that do not match
  the fields.

## 5. Security

- The pipe is the job's id's and `0600`, in a lease directory no container sees. Only the agent and that id
  reach it, and no job's process of another container sees the path.
- The init reads it once, before any job code runs in the container. Nothing reads it afterwards.
- The binds are made by root in the container's namespace before the job runs, with the same flags `crun` gives
  (`nosuid`, `nodev`, read-only as asked).
- The job holds no capability and cannot remount them, as in a cold container.
- The agent's thread `chroot`s into the container's root before it resolves a target. A link in the image
  therefore resolves inside the container, never on the host.

## 6. macOS

The native runtime has no container to make ahead. Its sandbox is a pool user, a Seatbelt profile and a home
directory, and the pool's users are already made ahead of jobs (`js-mac-pool`). What a job start still does
there is write its profile and copy its inputs, and both depend on the job. No warm pool is built for macOS.

## 7. Tests

- **Stand-in crun.** The stand-in `crun` plays a held container. It joins its group, reads the job from the
  lease's pipe, and plays the part the job's argument names. These tests are in
  `crates/js-agent/tests/crun_stand_in/warm.rs`:
  - a run in its image's sandbox, end to end;
  - runs the pool cannot carry;
  - a failed bind and a failed hand-over, each made cold;
  - the pool's steps: what ended, what is unused, and an image that rests;
  - a restart: an idle sandbox removed, a warm run adopted.
- **The pool's rules.** `crates/js-agent/src/crun/warm_tests.rs` covers sizing, eligibility, the binds and when
  a sandbox is up.
- **The init.** The init's `--wait-job` tests in `crates/js-contain/src/oci/init.rs` cover the job run in its
  directory and environment, malformed jobs and the usage rules.
- **Bundle and hand-over.** `crates/js-contain/src/oci/held_tests.rs` covers the held bundle and the hand-over.
- **The bind.** `crates/js-contain/src/job_fs/inject_tests.rs` covers the order of calls and each failure.

The kernel calls themselves need root and a real container, so they are covered by the farm's measurement, not by
the per-PR suites.

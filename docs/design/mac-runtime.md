# The macOS runtime: where it lives and what macOS enforces

Status: proposed. The macOS sandbox stays `js-agent`'s macOS backend, in this workspace. It is an account per job that the agent makes and deletes, the Seatbelt profile per run and pf per uid; the root helper `js-mac-session` it once had is retired (`mac-one-program.md`, section 13). It does not become a separate repository or a standalone OCI runtime: no one else would use it, and its only caller is the agent. This document fixes three things: the one interface both backends implement, the crate boundaries, and what macOS can and cannot enforce for each part of a job. Then it covers the change that does need building, memory and CPU limits, and the least macOS version the Macs' pool admits.

## What changes

Nothing in this document changes code by itself. Jobs, the wire, placement and the Mac package stay as they are. Section 5 sets one value of the Macs' pool file, its least `os_version`. Section 4 proposes the one behaviour change: today a Mac job is held to no memory or CPU limit. After section 4's steps, a run that goes over its memory cap is killed and reported as a memory kill, as on Linux. The agent already reports what each run uses (section 4, step 2).

## 1. One runtime interface

The interface exists: `trait Runtime` (`crates/js-agent/src/lifecycle/runtime.rs`). The lifecycle drives every backend through it, with `create_and_start`, `poll`, `stop`, `remove`, `list_labelled` and the defaulted checks.
- On Linux, `CrunRuntime` (`crates/js-agent/src/crun`) implements it through `js-contain` and crun.
- On macOS, `NativeRuntime` (`crates/js-agent/src/native`) implements it in the agent's own process, over the slot accounts (`crates/js-agent/src/mac_accounts`) and a supervisor of its own per run (`crates/js-agent/src/mac_supervisor`).

A separate change makes the backend a compile-time choice by target OS. Nothing more is needed at the interface: no second trait, no OCI bundle for macOS, no runtime binary between the agent and the job but the supervisor, which is the agent's own program.

## 2. Crate boundaries

The four macOS parts are one feature, the macOS backend. They are split where the process, the privilege or the purity changes, not by topic:

| Part | What it is | Why it stands alone | Change |
|---|---|---|---|
| `js-agent/src/native` | The `Runtime` implementation: run directory, input copies, cwd, env, outputs, logs, journal, the calls to the account pool and the supervisor | It is built from the lifecycle's types (`RunSpec`, `Status`, `Found`). Moving it out would mean moving the trait and those types to a lower crate for no other caller. | none |
| `js-mac-pool` | The Seatbelt profile builder and the Xcode catalog | It is a pure crate (R4): no clock, sockets or process starts, so its profile text and goldens are tested on Linux in every pull request | none |
| `js-mac-session` | A library crate with two modules: `disclaim` (the TCC-disclaiming start) and `full_disk_access` (the judgement of whether a process holds Full Disk Access). It ships no program | The one way the agent and `js-mac-provision` start a program, and the judgement the agent makes of its own grant, in a crate both can use. The root helper this row once described (the user pool and its ledger, starting a program as a pool user, `kill-uid`, the reset, the VM user, image set-up and the leak scan) is retired: the agent does that work in process (`mac_accounts`, `mac_sweep`, `mac_supervisor`) | retired as a program |
| `js-mac-provision` | The pf anchor (render, install, check; `install --flags-file` reads the agent's flags file), the two LaunchDaemon plists the package ships (the agent and the pf job) | It runs at install and at boot (the pf LaunchDaemon), before and beside the agent | none |

No crate needs to merge or split.

`disclaim` (the TCC-disclaiming start, `docs/design/spawn-disclaim.md`) is a module of `js-mac-session`, the library crate. The agent and `js-mac-provision` use it from there.

## 3. What macOS gives us

macOS has no namespaces and no cgroups. Each part of a job maps to the primitive below.
- **"Enforced today"** means the native runtime uses the primitive now.
- **"Check"** names what checks the claim. It is either an asserted `js-mac-probe` probe, which runs on the beta stage's macOS runner ("new" marks one that section 4 adds), or a root test that an operator runs on a provisioned Mac.

These facts were not re-checked on current macOS releases beyond those checks.

| Job field (OCI name) | macOS primitive | Enforced today | Check |
|---|---|---|---|
| process isolation (`linux.namespaces`) | A separate uid per run, an account the agent makes for the job and deletes after it. Seatbelt's `process-info` deny does not hide other users' processes from `ps`. | yes, the uid | root test `native_isolation` (two runs on one uid) |
| file access | Seatbelt (`sandbox-exec`). Reads are open except the read denies; writes are allowed only in the run directory `exec/` below the user's home (`HOME` and `TMPDIR` are inside it; the home itself is not writable). Endpoint Security could watch or authorise file events too, but it needs an entitlement Apple grants per developer, so it is not used. | yes | `sandbox/deny-write-outside`, `sandbox/deny-write-private-tmp` |
| `root`, layers | No overlay or bind mounts. The options are `clonefile` (same APFS volume, copy-on-write) and a per-run disk image. Layers are refused (`rootfs-unsupported-here`, `docs/design/job-rootfs.md` 5.1). | refused | `fs/clonefile-*` |
| `mounts` | Copies into the run's `exec/`, write-denied by the profile (`files.rs`). A job start's `READ_ONLY` and `READ_WRITE` mounts of host paths below `/work` are links in `exec/` with a Seatbelt grant of the canonical path, an exec deny when `no_exec`, and every mount root a read deny of every run (`job-rootfs.md` 5.2 and 5.4, built in `native/mounts.rs`); copy-on-write mounts are refused. | yes, read-only and read-write; copy-on-write refused | `sandbox/control-write-input`; the links, grants and read denies: `native::mounts` in `tests/agent/native/mounts.rs` |
| network | pf rules matching the pool's uids (TCP and UDP only; DNS through `mDNSResponder` is outside pf), plus Seatbelt for `off` and `loopback` (a `none` job gets loopback under every ceiling but `off`). Loopback is shared by every user of the host, its services included. | yes | `net/off-refuses-loopback`, `net/loopback-allows-loopback`; a `none` job on loopback: `js-agent/tests/native_privileged/loopback.rs`; pf by root test `pf_as_root` |
| `rlimits`: `RLIMIT_NPROC` | Counted per uid, so per run | yes (`--native-max-processes`, agent-wide) | new: `limits/nproc-per-uid` |
| `rlimits`: `RLIMIT_FSIZE` | Per process | yes (`--native-max-file-mib`, agent-wide) | new: `limits/fsize` |
| memory (`resources.memory`) | No cgroup. `setrlimit` accepts `RLIMIT_AS`, `RLIMIT_RSS` and `RLIMIT_DATA` but macOS does not enforce them. Jetsam limits are a private interface. What remains is reading each process's footprint (`proc_pid_rusage`, `ri_phys_footprint`) and killing the uid. | **no** | `proc/rusage-own-child` today; new: `limits/as-not-enforced`; `proc/rusage-other-processes`, an info probe today, becomes asserted and reads a process of another uid |
| CPU (`resources.cpu`) | No quota. `taskpolicy`, `setpriority` and the QoS classes change priority only. `RLIMIT_CPU` ends one process after a number of CPU seconds; it is not a rate. | **no** | new: `limits/cpu-seconds` |
| disk | No per-user quota on an APFS volume. A per-run disk image of fixed size would cap a run. Disk caps are not required here. | no | none |
| VMs | Virtualization.framework, with at most 2 macOS guests per host under Apple's licence (the VM path, `js-vmm`). Apple's Containerization runs Linux containers, each in a small VM, on macOS 26: Linux guests only, so it does not run macOS jobs. | the VM path | none |

## 4. Memory and CPU limits

**Today.** The scheduler books each Mac job's memory and CPU against the host, so bookings bound how many jobs share a Mac. Nothing holds a job to its booking. The native runtime ignores `memory_bytes` and `cpu_millis`. It reports what each run used, as a Linux run's usage is reported (step 2 below, `crates/js-agent/src/mac_usage`):
- At each poll of a running job (once a second), the agent reads `proc_pid_rusage` of every process of the run's account: the sum of their `ri_phys_footprint` is the run's memory at that moment, and the sum of their user and system time, with that of the children they waited for (`mach_timebase_info` converts the ticks), is its CPU. The run's CPU is the most that sum has been, and its peak the most footprint one sample saw.
- The supervisor reaps the job with `wait4`, and its exit record carries that usage (`usage` of `exit.json`, an optional field a record from an earlier supervisor lacks): the CPU of the job and of every descendant it waited for, and the largest resident set one of them reached. So a run too short for any sample still has both.
- The job's result carries the more of each as its `usage` and, with `--cpu-series` and `--memory-series`, the two series; a run with neither a sample nor a wait's usage carries none, never a zero.
- The heartbeat carries the Mac's load: its busy time from `host_statistics` (`HOST_CPU_LOAD_INFO`, widened across the 32-bit counters' wrap) and its non-file memory from `host_statistics64` (anonymous pages less purgeable ones, wired and compressed pages), as a Linux host's `/proc/stat` and `/proc/meminfo` are read; with `--host-second-samples`, the busy CPU of each second, and no pressure (a Mac has none).

**Memory: a cap enforced by sampling.**
- The agent samples each live run: it already sums the `ri_phys_footprint` of the run's processes at each poll (**Today** above), and the cap is checked against that sum.
- The cap is the Linux build-host policy, `booking × 1.5 + 512 MiB`. The function that computes it, `memory_max` in `crates/js-contain/src/cgroup.rs`, moves to a crate both targets build, so the two platforms cannot drift apart.
- A run over its cap is ended by the sweep of the run's uid (`crates/js-agent/src/mac_sweep`). `poll` reports it as `Exit { oom_killed: true }`, the result a Linux OOM kill gives.
- Between samples a run can overshoot. So this is a cap a run cannot hold for longer than one interval, not a hard ceiling. The host's backstop stays `memory_schedulable_percent`, as on Linux.
- The run's `memory_bytes` is already in the runtime's hands, so the cap needs no field on a wire and no fallback for an older peer: there is no second process to ask.

**CPU: measured, not limited.** macOS cannot hold a uid to a CPU share. CPU stays compressible, as Linux treats it (`cpu.weight`, never `cpu.max`): the bookings bound the jobs, and nothing throttles one.
- The same sample adds each run's CPU time (`ri_user_time` and `ri_system_time` of its processes, converted with `mach_timebase_info`, plus the CPU time of its exited children, which the supervisor's exit record carries).
- Memory and CPU go into the heartbeat as a Linux run's usage does. Rightsizing from history then works on Macs as on Linux.

**Steps, each shippable:**
1. The new asserted probes of section 3, in `js-mac-probe`.
2. Usage (done, as **Today** above says): the agent's sample of the run's processes at each poll and the supervisor's `wait4`, the job's usage and series from them, and the heartbeat's load from the host's counters.
3. The memory cap: `memory_cap_bytes`, the kill, and the `oom_killed` report.

Each step is tested on Linux against the fake account host of `js-agent`'s tests, and on the beta stage's macOS runner.

## 5. The least macOS version of the Macs' pool

**The farm.** The four Macs run three macOS releases: two run 27.0.1, one 26.6 and one 26.4. An agent's hello reports the major version alone (`crates/js-agent/src/os_version.rs`), so the server sees 27, 27, 26 and 26. A job asks for a least major version in `constraints.osVersion`, compared as at least.

**The policy: one pool with a floor.** The Macs stay in one pool, `mac-build`, and its `[capabilities]` name `os_version = "26"`:

```toml
[capabilities]
os = "macos"
arch = "arm64"
os_version = "26"
```

- The floor only admits. Each Mac is placed by the version its own hello reports, not by the pool's value (`placement_shape` in `crates/js-registry/src/config.rs`). So a job that asks for 27 is placed only on the two Macs on 27, and a job that asks for 26, or for no version, on all four.
- One pool per version would add nothing to placement and would cost more:
  - Every Mac is booked at what it reports in either pool, so a second pool would not book Macs differently; the version is the only thing that would differ.
  - A version is matched as at least, and a pool has no ceiling, so a pool for 26 would still admit a Mac on 27. It could not hold its hosts to 26.
  - Each extra pool needs its own join tokens and the same `memory_schedulable_percent`.
  - A Mac names its pool in its flags, so each macOS update would also move the Mac to another pool.
- 26 is the least version on the farm. The macOS runtime has been built and tested on 26.4, 26.6 and 27.0.1 only. A Mac below 26, or one whose hello reports no version (its probe failed, or its agent predates the report), is refused at its hello with `shape-not-accepted`, reason `below-floor`. That refusal shows in the agent's log, which is better than a Mac that joins and quietly serves only the jobs that ask for no version. This floor is the `os_version` capability of the pool's `[capabilities]`, a version and not an amount, and it stays: a Mac is never refused for reporting fewer cores or less memory than the pool's `[shape]`, and is booked at what it reports.
- Minor versions are not placed. 26.4 and 26.6 are the same version to the scheduler, and a job cannot ask for 26.6. No job needs that today.
- Raising the floor, once every Mac runs 27, is an edit of this value in the pool file, made after the Macs are updated. A Mac still below the new floor is refused at its next hello.

**Check.** `crates/js-lease-server/tests/server/mac_os_version.rs` runs a server over this pool with four Macs (two on 27, two on 26). Jobs that ask for 27 fill only the two Macs on 27. Jobs that ask for 26 fill all four. A job that asks for 28 is refused `no-capable-host`. A Mac on 25, or one that reports no version, is refused `below-floor`.

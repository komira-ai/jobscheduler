# Setting up a Linux host to run js-agent with crun (`crun_host_setup.sh`)

`js-agent --runtime crun` runs jobs with `crun` as an unprivileged job user, without a container
engine (a host job, which only `--allow-host-jobs` admits, runs as root: see
[host-jobs.md](host-jobs.md)). `crun` is the default Linux runtime: an agent given no `--runtime` (and no `--native-*`
flag) runs it too. Before it can start, the host needs a handful of pieces; the agent's own start
check names each one that is missing and exits 2. `crun_host_setup.sh` makes the pieces, once per
host.

The script is in the repository (`packaging/js-agent/crun_host_setup.sh`) and in the js-agent deb, at
`/usr/share/js-agent/crun_host_setup.sh` (mode 0755). The units and tmpfiles file it installs are
`packaging/js-agent/js-agent-crun.service`, `js-agent-crun-gpu.service` and `tmpfiles-crun.conf` of
the same release: it takes them by path and sha256, so what lands in `/etc` is the file you checked.

## A host that fails the check

The agent never falls back to the container engine when the check fails: it exits 2 before it
registers, naming every missing piece and what to do about it (the unit's journal shows the
list: `journalctl -u js-agent-crun` for the packaged crun unit). An agent that was given no
`--runtime` is also told that `crun` is the default and of the two ways forward:

- Fix the pieces. Run `crun_host_setup.sh` (below), or fix each named piece by hand. Two pieces
  are what a host most often lacks: the AppArmor profile for `/usr/bin/crun` where
  `kernel.apparmor_restrict_unprivileged_userns` is 1 (`apparmor profile` in the list), and the
  cgroup version 2 controllers `cpu`, `memory` and `pids` offered to the agent's cgroup, which a
  unit with `Delegate=yes` gets (`cgroup v2 delegation` in the list).
- Opt in to the engine, on a host that cannot run `crun` or that runs GPU jobs: give
  `--runtime docker --container-bin <path> --allow-container-engine` (the packaged
  `js-agent.service` does). Every job the engine runs gets network none, and the agent prints one
  warning at start that says so. `--runtime docker` without `--allow-container-engine` exits 2.

## What it sets up

The steps, in order. The `step` and `what` columns are also `packaging/js-agent/crun_host_steps.toml`,
the list the machine image build follows (`docs/runbooks/machine-images.md`); a test fails when
this table, the script's order or that file differ.

| step | what | when it is already so |
|---|---|---|
| packages | `crun` (1.14 or newer, owned by the distribution's package at `/usr/bin/crun`) and `uidmap` from apt | installed: left |
| job user | a system account (default `jsjob`), no login, no home | exists with a non-zero uid and gid: left, never edited |
| subids | a block of 65536 ids in `/etc/subuid` and `/etc/subgid` for that user, the first free block from 200000 that overlaps no range, uid or gid on the host | both files already give the user 65536 ids or more: left |
| scratch quota | `prjquota` added to the `Options=` of the scratch mount unit (the options it has are kept), then the filesystem is unmounted and mounted again through the unit | the unit has it and `findmnt` shows it: left |
| directories | `/etc/js-agent` 0700, `/var/lib/js-agent` 0711, its `state` 0700, `scratch` 0711 and `scratch/runs` 0711, all root's | right owner and mode: left; another mode is changed and the old one recorded; a directory not root's, or a link, is refused |
| AppArmor | where `kernel.apparmor_restrict_unprivileged_userns` is 1, the distribution's profile for `/usr/bin/crun` is loaded | loaded, or the sysctl is not 1: left |
| unit | `/etc/systemd/system/js-agent-crun.service` (with `--server`, `--pool` and `--job-user` written in), `/etc/systemd/system/js-agent-crun-gpu.service` (the GPU flags' oneshot, see [GPU hosts](#gpu-hosts)) and `/etc/tmpfiles.d/js-agent-crun.conf` | same bytes: left; a file that differs is never overwritten |

It does not enable or start the units. Switching from `js-agent.service` to `js-agent-crun.service` is a
step of its own, after the agent binary prints `runtimes: docker crun` in `--version` (the script
says whether it does).

## How to run it

The host must be drained, and the agent stopped (`js-agent.service` and `js-agent-crun.service` both
inactive or failed): the script refuses otherwise, because it changes the modes of the agent's
directories and mounts the scratch filesystem again.

```text
# dry run, the default: reads the host, prints the plan, changes nothing
crun_host_setup.sh --unit js-agent-crun.service --unit-sha256 <sha256> \
    --gpu-unit js-agent-crun-gpu.service --gpu-unit-sha256 <sha256> \
    --tmpfiles tmpfiles-crun.conf --tmpfiles-sha256 <sha256>

# apply: the same checks, then the changes
crun_host_setup.sh --apply --server https://scheduler.example.org:7300 --pool farm \
    --unit js-agent-crun.service --unit-sha256 <sha256> \
    --gpu-unit js-agent-crun-gpu.service --gpu-unit-sha256 <sha256> \
    --tmpfiles tmpfiles-crun.conf --tmpfiles-sha256 <sha256>
```

| flag | meaning |
|---|---|
| `--apply` | do it; without it the run is a dry run |
| `--job-user NAME` | the job user (default `jsjob`; the unit names it) |
| `--subid-start N` | first id of the subid block (default: the first free block from 200000) |
| `--unit FILE`, `--unit-sha256 HEX` | `js-agent-crun.service`; an apply needs both |
| `--gpu-unit FILE`, `--gpu-unit-sha256 HEX` | `js-agent-crun-gpu.service`; an apply needs both |
| `--tmpfiles FILE`, `--tmpfiles-sha256 HEX` | `tmpfiles-crun.conf`; an apply needs both |
| `--server URL`, `--pool NAME` | written into the unit in place of its placeholders; an apply needs both |
| `--scratch-mount-unit NAME` | the mount unit of `/var/lib/js-agent/scratch`, a file in `/etc/systemd/system` (default `var-lib-js\x2dagent-scratch.mount`) |
| `--dependents-limit N` | how many units the walk of the mount's dependents may look at before it refuses (default 1000) |
| `--skip-scratch-quota` | leave the scratch mount as it is, for a host whose scratch is not a mount unit |
| `--root DIR` | run against the tree `DIR` instead of `/`; for tests only. The programs it runs still act on the host, and the script's root check (`id -u`) still wants euid 0 for `--apply`, while the guard against acting on the host wants the real uid (`id -ur`) not to be 0: so `--apply` with `--root` can work only with euid 0 and ruid not 0, which no ordinary login has |

The script reads no environment variable. Exit codes: 0 done, or a plan; 1 a step failed; 2 refused or
bad usage, with nothing changed; 3 nothing to do.

Every refusal is found before the first write: a dry run shows the same refusals an apply would.
A rerun on a host that is set up changes nothing and exits 3, so it is safe to run again after an
interruption or to check a host.

## When the agent stops or crashes

Each job's container runs under a supervisor process of its own (the agent's binary in a helper
mode, as the job user, in the agent's control group but its own process group). The supervisor
holds the container's output and, when the container ends, writes how it ended to `exit.json` in
the lease's directory. The unit's `KillMode=process` lets systemd kill only the agent's main
process, on a stop and after a crash, so the supervisors and the containers outlive the agent.

- **A stop** (`systemctl stop`, an update) still drains: the agent lets its jobs run for up to
  `--drain-wait` seconds, stops them within `--stop-grace`, reports, and exits.
- **A crash** leaves the jobs running. When `Restart=always` starts the agent again, it reads its
  ledger and takes over (adopts) each container its record holds, then lists those jobs in its
  first heartbeat. A job that ended while no agent ran is reported with the code and output tail
  from `exit.json`. Anything of the agent's that it cannot take over is removed and logged
  (`js-agent: warning: crun: removed the lease of job <id>: <reason>`), and the server finds that
  job lost because the agent no longer lists it. Each job taken over is logged too
  (`js-agent: warning: crun: adopted job <id>`).
- **A job's wall limit** counts the time the job ran with no agent, but only a running agent stops
  a job: one whose limit passed while no agent ran is stopped at the new agent's first step, and
  its result says it timed out.
- **An agent with an allow-list `--egress`** takes over no container that may still run: its
  start removes every network wiring an earlier process made, so it removes those containers
  instead, and their jobs are lost. A container that ended while no agent ran has no network
  left: it is taken over and its result reported, as above.

The ledger is one file per lease, `<image store>/oci-labels/<lease>.json` (the image store is
`oci` beside the scratch root), mode 0600, root's. Version 2 of the file holds:

| field | what |
|---|---|
| `version` | `2`; a version 1 file (an older agent's) is not taken over: its lease is removed |
| `agent`, `run_id`, `name`, `labels` | the agent's id, the job id, the lease's name, the container's labels |
| `state` | `starting` until the supervisor runs, then `running`; a lease left `starting` is removed |
| `allocation` | the allocation (lease) id the start carried |
| `booked` | `cpu_millis`, `memory_bytes`, `disk_bytes`, `gpus`, `slot`: what the job booked |
| `started_wall_ms`, `wall_ms` | when the container started (wall clock) and its wall limit; the new agent gives the job the time left, never more than `wall_ms` |
| `lease_dir`, `cgroup`, `scratch`, `exit_record` | the lease's directory, its control group under `<unit>/actions`, the job's scratch directory, and the supervisor's `exit.json` |
| `image_digest` | the digest of the job's image |
| `supervisor` | the supervisor's `pid` and `start` (start time in clock ticks): together they name the process |

A lease is taken over only when the file is version 2 and in state `running`, its labels agree with
its fields, its directories and control group are where the agent puts them (not links), and
either `exit.json` is there or `crun state` names the container's first process in the lease's
control group and the supervisor still runs.

A job whose logs go to an object store also has `logs-upload.json` in its lease directory, mode
0600, the agent's: the progress record of its upload. It names where the logs go (the store, the
bucket and the job's prefix; no credential, which the agent holds in memory only) and, for each
stream, how far the store has it. A restarted agent goes on with the upload from it, and uploads
what the job wrote while no agent ran; a job that ended meanwhile gets its final logs written. A
record that is a link, not the agent's own file, of another version than 1, or of another job, is
ignored with a warning, and the job is adopted without live logs: its output stays on the host.
The record goes with the lease directory.

Results the server has not acknowledged, and the job ids the agent must not run again, are kept in
the state root: one file per held result in `held/`, and `retired.json` (job id and allocation id
pairs, the newest 4096). A file there that cannot be read is dropped and logged as a finding; it
does not stop the agent from starting.

## GPU hosts

The script installs no driver. On a host whose GPUs the agent should give its jobs, install the
driver first (the host's software lifecycle does that, not the agent), and copy the driver's
user-space libraries and programs for that version into `/opt/nvidia-driver/lib64` and
`/opt/nvidia-driver/bin` (real files: a link to a file outside the directory does not resolve in
the job's container), so that nothing else of the host's library directory is given to the job.

The GPU flags are generated. `js-agent-crun.service` pulls in `js-agent-crun-gpu.service`
(`Wants=`, `After=`), a oneshot that runs before each start of the crun unit:

1. `js-agent crun-gpu-dropin` runs `/usr/bin/nvidia-smi -L`, which also makes the driver create
   the nodes of the GPUs and `nvidiactl`, and reads the GPUs it lists. The oneshot runs after
   `systemd-modules-load.service`, `systemd-udev-settle.service` and `nvidia-persistenced.service`
   where the host has them, so the driver's other nodes (`nvidia-uvm` and the rest) are made by
   then; a node that is not there yet is not given to jobs;
2. it lists `/dev`: `nvidia<N>` (one per GPU), `nvidiactl`, `nvidia-modeset`, `nvidia-uvm`,
   `nvidia-uvm-tools` and the entries of `nvidia-caps/`. GPUs listed by `nvidia-smi -L` that
   differ from the `nvidia<N>` nodes are a warning in the oneshot's journal; the check in step 4
   then uses `nvidia-smi`'s list, which is the one the agent checks at start, so a GPU it lists
   with no node is refused;
3. it reads the crun unit's `ExecStart` (`/etc/systemd/system/js-agent-crun.service` and the
   drop-ins of `/etc/systemd/system/js-agent-crun.service.d/` and
   `/run/systemd/system/js-agent-crun.service.d/`, by name, so a `systemctl edit` of the server or
   pool is kept; a quoted word is refused) and adds:

   ```text
   --gpu-device /dev/nvidia{index} --gpu-device /dev/<each other node, in name order> \
   --gpu-mount /opt/nvidia-driver/lib64:/usr/local/nvidia/lib64 \
   --gpu-mount /opt/nvidia-driver/bin:/usr/local/nvidia/bin \
   --gpu-env LD_LIBRARY_PATH=/usr/local/nvidia/lib64 \
   --gpu-env PATH=/usr/local/nvidia/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
   ```

4. it checks the whole command line as the agent's start does (the agent's parser, then the GPU
   flags' check against the nodes and the driver directories), and only then writes
   `/run/systemd/system/js-agent-crun.service.d/zz-gpu.conf`: `[Service]`, an `ExecStart=` reset
   and the command line. The name sorts after `override.conf`, the drop-in `systemctl edit`
   writes, so its `ExecStart` is the one that applies;
5. the oneshot reloads systemd, and the crun unit starts with the flags.

It writes nothing, and removes a drop-in an earlier start wrote, when the host has no GPU node and
`nvidia-smi -L` lists no GPU (a host without a GPU starts as the unit says), when the crun unit's `ExecStart` lists a `--gpu-` flag
already, and when a step fails: the check refuses the flags (a missing driver directory, say), the
unit cannot be read, or the drop-in cannot be written. The agent then starts without GPUs, never
with flags it would refuse at start; `journalctl -u js-agent-crun-gpu` says why. The drop-in is in
`/run`, so after a reboot it is made again from the nodes the host has then.

A manual override stays possible: put the flags in the crun unit's `ExecStart` (with `systemctl
edit js-agent-crun`, clearing `ExecStart=` first), and the generator leaves the unit to them. What
the flags mean:

- `--gpu-device PATH`, repeatable: a character device below `/dev`. A path with `{index}` is one
  GPU's node and is given only for the indices the job booked; a path without it is given to every
  GPU job. The job's device cgroup allows these nodes and denies every other device.
- `--gpu-mount SRC:DST`, repeatable: a directory bound read-only (`nosuid`, `nodev`) at `DST`,
  not recursively: a mount below the directory on the host does not reach the job. A file source
  is refused: binding a file where the image has none would leave a file in the image's root that
  every job of that image shares. `DST` may not be at or below `/proc`, `/sys`, `/dev`,
  `/scratch`, another mount's destination, or a place the runtime mounts itself (`/work`, `/root`,
  `/tmp`, `/var/tmp`, `/run`, `/etc/hosts`, `/etc/hostname`, `/etc/resolv.conf`), nor hold one of
  them (`/etc`, `/var`). A job whose
  image has a link or a file on the way to `DST`, or whose driver directory is gone, is refused
  when it starts, naming `--gpu-mount`; so pick a destination the image does not have, such as
  `/usr/local/nvidia/lib64`.
- `--gpu-env NAME=VALUE`, repeatable: a variable set over the image's of that name (one entry,
  however many the image has).

A job that books no GPU gets none of it. At start every node must be a character device, for every
GPU the host offers (`--gpus` lowers that), or the agent exits 2 naming the flag; with the flags in
place the hello reports `gpu-passthrough`. Without `--gpu-device` a start that books a GPU is
refused `gpu-unsupported`, as before.

**The job image must contain the CUDA user-space libraries (the CUDA runtime, and cuBLAS, cuDNN or
whatever else the job links) of a CUDA version the mounted driver supports.** The host gives the
job only the driver's own user space (`libcuda.so`, `libnvidia-ml.so` and the driver's other
libraries at `/usr/local/nvidia/lib64`, its programs such as `nvidia-smi` at
`/usr/local/nvidia/bin`), never the CUDA toolkit: a job whose image has no CUDA runtime, or one
newer than the driver supports, sees the GPU's nodes and fails when it initialises CUDA. `PATH`
replaces the image's own, so it lists the image's usual directories after the driver's.

A pool whose `exec` is `container` and whose shape has GPUs also asks for `gpu-toolkit`, which only
the engine runtime's `--gpu-toolkit-hook` reports: such a pool does not admit a crun host yet.

## The scratch quota

The agent puts a project quota on each run's directory, which needs the scratch XFS filesystem
mounted with `prjquota`. XFS does not turn quota accounting on by remounting, so the script
unmounts and mounts the filesystem again through its unit: it edits `Options=` (for example
`loop` becomes `loop,prjquota`), runs `systemctl daemon-reload`, `systemctl stop` and
`systemctl start` on the mount unit, and then checks with `findmnt -o OPTIONS` that `prjquota` is
active, failing (exit 1) if it is not.

It refuses, before changing anything, when

- something holds the mount open (`fuser -m` finds a process): drain the host and stop what uses
  it first;
- the unit has no `Options=` line or several in its `[Mount]` section, so the options to keep
  cannot be told, or its `Type=` is not `xfs`;
- the unit file is missing (name it with `--scratch-mount-unit`, or leave the quota out with
  `--skip-scratch-quota`).

Before the unmount it also refuses when

- `fuser` fails or complains (only a clean "nothing holds it" counts);
- the filesystem is mounted anywhere else (a bind mount keeps it alive, so prjquota would not
  take) or something is mounted below the scratch directory;
- another active unit would stop with the mount unit: one that requires it, is bound to it or is
  part of it, or depends on one of those, followed through (`systemctl show -p
  RequiredBy,RequisiteOf,BoundBy,ConsistsOf,PropagatesStopTo`). A custom target counts. Only
  `local-fs.target` is left out: a mount that `local-fs.target` requires takes it down when it
  stops, and starting the mount does not bring it back (it starts again when something requires
  it). A walk longer than `--dependents-limit` units (default 1000) is refused, not passed;
- `<unit>.d` exists (a drop-in can override `Options=`), or the `Options=` line is continued with a
  backslash.

After the edit and the reload it checks `systemctl show -p Options` for prjquota before it
unmounts anything. If the mount does not start again after it was stopped, the run stops there,
leaves the host drained and prints `systemctl start <mount unit>`; no later step runs, so nothing is
made on the root filesystem under the mount point.

Where the scratch filesystem is not mounted at the moment, only the unit is edited and the option
applies at its next mount.

## What it records

`/var/lib/js-crun-host-setup/manifest` (root, 0600) has one tab-separated line for each thing an
apply created or changed: `pkg`, `user`, `subuid`, `subgid`, `mount-options` (the file, the
`Options=` it had, the sha256 of what the script wrote), `dir-created`, `dir-mode` (the old mode),
`apparmor-loaded`, `file` and `reload`; a run that ends in a failure or a signal adds
`incomplete`, the exit status and what failed first (the command, or the step when no command failed). The state directory must be 0700 and the
caller's, or the run is refused. The script has no rollback verb; each line says what to undo
by hand. Undoing `mount-options` means putting the old `Options=` back and mounting the filesystem
again.

## How it is tested

`crates/xtask/tests/crun_host_setup.rs` runs the script against a fake root, with a `PATH` of
recording stand-ins (`crates/xtask/src/bin/host-shim.rs`) for `systemctl`, `mount`, `umount`,
`findmnt`, `fuser`, `apt-get`, `useradd`, `usermod` and the other programs that read or change the
host. A stand-in nobody configured fails the call, and the tests refuse to run as root or when a
real program of the host could be reached from the script's `PATH`. The real `systemd`, `mount` and
`apt` are not exercised: the first host a person sets up is where they are.

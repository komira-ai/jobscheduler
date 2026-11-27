# Setting up a Linux host to run js-agent with crun (`crun_host_setup.sh`)

The Linux build of `js-agent` runs jobs with `crun` as an unprivileged job user, without a
container engine (a host job, which only `--allow-host-jobs` admits, runs as root: see
[host-jobs.md](host-jobs.md)). `crun` is the runtime of the Linux build: the agent has no
`--runtime` flag, and an agent given neither `--allow-container-engine` nor `--no-jobs` runs it.
Before it can start, the host needs a handful of pieces; the agent's own start
check names each one that is missing and exits 2. `crun_host_setup.sh` makes the pieces, once per
host.

The script is in the repository (`packaging/js-agent/crun_host_setup.sh`) and in the js-agent deb, at
`/usr/share/js-agent/crun_host_setup.sh` (mode 0755). The units and tmpfiles file it installs are
`packaging/js-agent/js-agent-crun.service`, `js-agent-crun-gpu.service` and `tmpfiles-crun.conf` of
the same release: it takes them by path and sha256, so what lands in `/etc` is the file you checked.

## A host that fails the check

The agent never falls back to the container engine when the check fails: it exits 2 before it
registers, naming every missing piece and what to do about it (the unit's journal shows the
list: `journalctl -u js-agent-crun` for the packaged crun unit), then says that the runtime of
this build is `crun` and names the two ways forward:

- Fix the pieces. Run `crun_host_setup.sh` (below), or fix each named piece by hand. Two pieces
  are what a host most often lacks: the AppArmor profile for `/usr/bin/crun` where
  `kernel.apparmor_restrict_unprivileged_userns` is 1 (`apparmor profile` in the list), and the
  cgroup version 2 controllers `cpu`, `memory` and `pids` offered to the agent's cgroup, which a
  unit with `Delegate=yes` gets (`cgroup v2 delegation` in the list).
- Opt in to the engine, on a host that cannot run `crun` or that runs GPU jobs: give
  `--allow-container-engine --container-bin <path>` (the packaged `js-agent.service` does).
  Every job the engine runs gets network none, and the agent prints one warning at start that
  says so. `--container-bin` without `--allow-container-engine` exits 2, and so does
  `--allow-container-engine` without `--container-bin`.

## Updating a host to an agent without `--runtime`

An agent from before the runtime was the build's own took `--runtime crun`, `--runtime docker`
and `--runtime none`. The agent now refuses `--runtime` by name and exits 2 at start, so a host
whose flags still name it does not start the new agent. The packaged units of the same release
name no `--runtime` and move with the binary, but the flags a host keeps elsewhere do not. Before
the update reaches a host, look for the flag where an operator may have put it:

```
grep -n -e '--runtime' /etc/js-agent/flags /etc/systemd/system/js-agent*.service.d/*.conf
```

Drop `--runtime crun`; replace `--runtime docker --allow-container-engine` with
`--allow-container-engine` (beside `--container-bin`); replace `--runtime none` with `--no-jobs`.
A host the update reaches with the flag still there fails the update at its cutover, as for
any release whose agent does not start.

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

- **A plain stop** (`systemctl stop`) drains: the agent lets its jobs run for up to
  `--drain-wait` seconds, stops them within `--stop-grace`, reports, and exits.
- **An update** hands the jobs over instead (see "Agent updates while jobs run" below): the agent
  hands over in place, or exits within seconds, leaving every job running, and the new agent takes
  them over as after a crash.
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
- **An agent with an `--egress` ceiling** (`allow-list` or `open`) takes over a container's
  network wiring with the container. The agent keeps one record per wired run in the egress
  store, the directory `egress` of its state root (`/var/lib/js-agent/state/egress`), written
  when it wires the run and removed when it takes the wiring down: the run's address block, its
  level and its declared ports. The stopping agent leaves each run's veth pair, set member and
  port forwards in place, and the records with them, so the kernel's rules enforce with no agent
  running. The next agent's start reads the records, loads the firewall in one transaction that
  puts each recorded run's member and forwards back (so the job's egress holds before, during
  and after the restart), and then takes over each run from its record after comparing the
  level and ports in it with those the container's labels (`jobscheduler.dev/network`,
  `jobscheduler.dev/network-ports`) give. It removes the wiring and the record of every run no
  one took over, any record it cannot read or use, and any `jsv`/`jsp` interface no record
  names. A container that does not fit (no network label, no record or a record that differs, a
  level above the agent's ceiling, a port the ceiling does not list, or an agent with no ceiling
  and a container wired at a level above none) is removed and its job lost, as any container the
  agent cannot take over. A container that ended while no agent ran has no network left: it is
  taken over and its result reported, as above. What is not handed over: the host-port relay
  connections (`--host-port`) the agent process itself carries end with it, and the next agent
  offers them again.

## Agent updates while jobs run

An agent update does not wait for the node to be idle, and it stops no job. hostkeeper runs the
update hook's steps; on a crun node they go like this:

1. `prepare` checks the new release and records that this update hands the jobs over. It places
   no cordon and waits for nothing: the node keeps taking work.
2. `cutover` writes the file `handover` in the agent's state root (`/var/lib/js-agent/state`) and
   then either signals the agent (in place) or stops the unit.
   - In place (the running and the new agent both list `in-place` on the `stops:` line of
     `js-agent --version`): the hook sends SIGHUP to the unit's main process
     (`systemctl kill --kill-whom=main --signal=SIGHUP -- js-agent-crun.service`). The agent
     reads the file, takes no new job and says `draining` to the server, waits until no job is
     being started (an image still being pulled counts), no job is being stopped and no finished
     job's cleanup and post-job checks are under way, hands the jobs over, and runs the program at
     its path (`/usr/bin/js-agent`, which the package replaced) again with its own arguments, as
     the same process. The unit is not stopped and not started, so systemd places no process in
     it. The new program takes the jobs over and removes the file at its start; the hook waits
     for that. A unit that is no longer active meanwhile (the agent ended instead of running
     again) is started as below. A request not taken in the cutover's stop bound fails the step
     (`stop_failed`, class environment, the file left for the agent).
   - By stop and start: the agent reads the file when the stop reaches it, does the same, and
     exits, leaving every running job as it is; that takes seconds. The hook removes the file
     and starts the unit on the new release.
3. The new agent takes over (adopts) every job, as after a crash, and lists them in its first
   heartbeat. A job that ended in the gap is reported from its `exit.json`, once. A job's result
   that the old agent had not had acknowledged is kept in the state root and sent by the new
   agent, once. Its first hello waits until the server stops counting the old session as live
   (the pool's grace, 15 s by default), so the node takes no new job for that long; the jobs
   that run are not touched by it.
4. `verify` waits for the new agent's hello, and `cleanup` ends the update. A `rollback` hands
   the jobs back to the previous agent the same way: no drain, the request file, and a signal
   (or a stop and a start). If the previous agent's unit then does not start, the report says
   that the previous release is installed again but its agent is not running, and the host runs
   no agent until the unit starts. A cutover whose start fails says the same of the staged
   release.

The update hands over only when all of these hold; otherwise it drains as before (cordon, wait
for no lease, stop):

- the runtime is crun and the unit manager systemd;
- the new release's unit is the same as the installed one (a changed unit must start from an
  empty cgroup, which a running job keeps busy);
- on a host whose flags file sets `--egress allow-list` or `open`, both the running agent (which
  leaves the wiring in place) and the new release's agent (which adopts it) list
  `egress-handover` on the `stops:` line of `js-agent --version`; a host whose flags file cannot be
  read is treated as having a ceiling. An `--egress` given only on the unit's command line is not
  seen by the hook, and such an agent does not read the file: it drains, and its jobs are
  stopped once `--drain-wait` has passed;
- the running agent can hand over: `js-agent --version` prints `stops: drain handover` (and
  `egress-handover`, as above). The first update from a release without that line still drains;
- the hand-over is made in place, or the host's systemd is older than 258: both the running and
  the new agent list `in-place` on the `stops:` line, or `systemctl --version` prints a version
  below 258. A systemd of 258 or newer (or one whose version cannot be read) starts a service's
  first process in the unit's own cgroup, which the jobs a stop leaves keep from taking one (see
  below), so an update that would stop and start the unit drains instead, and its detail says
  why.

What does not survive the restart's few seconds: a job's `--host-port` listeners, and the
connections open through them, are the agent's and end with it; they are down until the new
agent offers the ports again (a job that took the port itself meanwhile is stopped, its result
`host-port-lost`). The CPU and memory use of a job taken over is no longer sampled.

The maintenance `drain` step (an OS update or a reboot) is not an agent update: it still cordons
the node and waits until no job runs.

### A start refused while jobs are left (systemd 258 or newer)

Since systemd 258 a service's `ExecStart=` process is cloned straight into the unit's own cgroup
and moved into its `DelegateSubgroup=` only after; 255 to 257 cloned it into the subgroup. The
kernel refuses a process in a cgroup that has controllers enabled for its children, and systemd
cannot switch the controllers off while the unit's `actions` cgroup, which holds the lease
cgroups of the jobs, uses them. So when the agent has ended and left jobs running (after a
hand-over by stop and start, or after a crash under `KillMode=process`), every start of
`js-agent-crun.service` on systemd 258 or newer fails until those jobs end: the journal says
`Failed to spawn executor: Device or resource busy` and the unit's result is `resources`. With no
job left the start works. An update avoids this by handing over in place or by draining (above).

The unit's `ExecStopPost=` step, `js-agent crun-start-refused --result ${SERVICE_RESULT}`, runs
after such a refused start. When the result is `resources`, systemd is 258 or newer and the
unit's cgroup has controllers enabled, it ends the jobs left (the supervisors first, then the
lease cgroups), so that systemd can trim the cgroup and the next restart (`Restart=always`)
starts the agent. Those jobs are lost; the server finds them lost because the new agent does not
list them.

By hand, on a node in that state: do nothing, and the unit comes back by itself within
`RestartSec` after the last job left ends. To bring it back at once, at the cost of those jobs:

```
systemctl kill --kill-whom=all --signal=SIGKILL js-agent-crun.service
```

after which the next restart starts it.

### An update record a rollback did not finish

The update hook keeps its record of an update in `/var/lib/js-agent-update/record.json` (root's;
on a Mac `/Library/Application Support/js-agent/update/record.json`) and deletes it when the
update ends, by `cleanup` or by a `rollback` that saw the previous agent's hello. A rollback whose
start was refused (above) ends before that hello and leaves the record, although the previous
agent comes back by itself once the last job ends. The next update names another target, and
its steps treat the record so:

- `prepare` finishes the record itself when the rollback is complete in effect: the record
  returns the host to the previous release this update names, the release link is on that
  release, the unit is active, the record does not say the agent that runs is the one the
  failed update started, and the lease server shows the previous agent's live session (a
  session it shows with no version, as it does for a hello a release that does not keep the
  version in the host's row took, counts by the session; one that names a version must name
  the previous one). It moves
  the record aside as `record.rolled-back-<target>-<ms>.json` in the same directory, says so in
  its detail, and goes on. Otherwise it fails `state_invalid` with the record kept, and the
  detail names the first thing left (`... its rollback is not complete: <what>; the rollback step
  finishes it`).
- the updater's `rollback` that follows rolls back the update on record, with the record's own
  target, previous version, digest and releases, and its detail begins `the record is of the
  update to <target>, not <ordered>`. It is refused, record kept, when the record returns the
  host to another previous version or release than the order's, and for a first install's
  record.

A hook from before this behaviour refuses such a record at both steps. To finish it by hand with
jobs running, run that hook's `rollback` with a params file that is the package's `[params]`
table plus the record's own `updater.target_version`, `updater.previous_version`,
`updater.staged_release`, `updater.previous_release` and `updater.artifact_digest` (the values
of its `update` object, byte for byte). With the unit active on the previous release it makes no
act on the unit: it waits for the previous agent's session, deletes the record and prints
`{"class":"release","promotion_pending":false,"reversible":false}`.
Such a hook refuses a session the lease server shows with no version (`version_mismatch: the
session is of no version on container, ...`), having changed nothing. When that happens and the
unit is active with the release link on the previous release, move the record aside under the
hook's lock, keeping its bytes, as the newer hook does:
`flock -n /var/lib/js-agent-update mv -n /var/lib/js-agent-update/record.json
/var/lib/js-agent-update/record.rolled-back-<target>-manual.json`.

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
pairs, the newest 4096) with `retired.log`, the ids retired since `retired.json` was last written
whole, one JSON object a line. An id is appended to the log while the log stays within a
sixteenth of `retired.json`; the next one writes `retired.json` whole and removes the log, so a
full list (some 370 KB) is rewritten about once in 250 job ends instead of at every one. A file
there that cannot be read is dropped and logged as a finding; it does not stop the agent from
starting.

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

The agent puts a project quota on each job's `/tmp` and `/var/tmp` (and, with `--scratch-quota`,
on each run's directory), which needs the scratch XFS filesystem mounted with `prjquota`: a crun
agent whose scratch filesystem lacks it starts no job. `--skip-scratch-quota` is only for a host
whose scratch is already mounted with `prjquota` some other way. XFS does not turn quota accounting on by remounting, so the script
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

After the edit and the reload, before it unmounts anything, it checks that systemd will mount
with the edited options: `systemctl show -p FragmentPath,DropInPaths` must name the edited file
and no drop-in, and the `Options=` of the text systemd loaded (`systemctl cat`) must have
prjquota; otherwise the run fails (exit 1). It does not read `systemctl show -p Options`: for a
mounted unit that gives the live mount's options, which lack prjquota until the remount. The
`findmnt` check after the mount is what proves the option took.

A rerun after a run that stopped at this check, with the unit already edited, does not edit it
again: it reloads, checks, and goes on to the remount.

If the mount does not start again after it was stopped, the run stops there, leaves the host
drained and prints `systemctl start '<mount unit>'`, the unit in single quotes so that the pasted
line starts it (a shell would drop the backslash of `\x2d` unquoted); no later step runs, so
nothing is made on the root filesystem under the mount point.

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

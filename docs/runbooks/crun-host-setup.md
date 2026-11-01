# Setting up a Linux host to run js-agent with crun (`crun_host_setup.sh`)

`js-agent --runtime crun` runs jobs with `crun` as an unprivileged job user, without a container
engine. Before it can start, the host needs a handful of pieces; the agent's own start check names
each one that is missing and exits 2. `crun_host_setup.sh` makes the pieces, once per host.

The script is in the repository (`packaging/js-agent/crun_host_setup.sh`) and in the js-agent deb, at
`/usr/share/js-agent/crun_host_setup.sh` (mode 0755). The unit and tmpfiles file it installs are
`packaging/js-agent/js-agent-crun.service` and `tmpfiles-crun.conf` of the same release: it takes
them by path and sha256, so what lands in `/etc` is the file you checked.

## What it sets up

| step | what | when it is already so |
|---|---|---|
| packages | `crun` (1.14 or newer, owned by the distribution's package at `/usr/bin/crun`) and `uidmap` from apt | installed: left |
| job user | a system account (default `jsjob`), no login, no home | exists with a non-zero uid and gid: left, never edited |
| subids | a block of 65536 ids in `/etc/subuid` and `/etc/subgid` for that user, the first free block from 200000 that overlaps no range, uid or gid on the host | both files already give the user 65536 ids or more: left |
| scratch quota | `prjquota` added to the `Options=` of the scratch mount unit (the options it has are kept), then the filesystem is unmounted and mounted again through the unit | the unit has it and `findmnt` shows it: left |
| directories | `/etc/js-agent` 0700, `/var/lib/js-agent` 0711, its `state` 0700, `scratch` 0711 and `scratch/runs` 0711, all root's | right owner and mode: left; another mode is changed and the old one recorded; a directory not root's, or a link, is refused |
| AppArmor | where `kernel.apparmor_restrict_unprivileged_userns` is 1, the distribution's profile for `/usr/bin/crun` is loaded | loaded, or the sysctl is not 1: left |
| unit | `/etc/systemd/system/js-agent-crun.service` (with `--server`, `--pool` and `--job-user` written in) and `/etc/tmpfiles.d/js-agent-crun.conf` | same bytes: left; a file that differs is never overwritten |

It does not enable or start the unit. Switching from `js-agent.service` to `js-agent-crun.service` is a
step of its own, after the agent binary prints `runtimes: docker crun` in `--version` (the script
says whether it does).

## How to run it

The host must be drained, and the agent stopped (`js-agent.service` and `js-agent-crun.service` both
inactive or failed): the script refuses otherwise, because it changes the modes of the agent's
directories and mounts the scratch filesystem again.

```text
# dry run, the default: reads the host, prints the plan, changes nothing
crun_host_setup.sh --unit js-agent-crun.service --unit-sha256 <sha256> \
    --tmpfiles tmpfiles-crun.conf --tmpfiles-sha256 <sha256>

# apply: the same checks, then the changes
crun_host_setup.sh --apply --server https://scheduler.example.org:7300 --pool farm \
    --unit js-agent-crun.service --unit-sha256 <sha256> \
    --tmpfiles tmpfiles-crun.conf --tmpfiles-sha256 <sha256>
```

| flag | meaning |
|---|---|
| `--apply` | do it; without it the run is a dry run |
| `--job-user NAME` | the job user (default `jsjob`; the unit names it) |
| `--subid-start N` | first id of the subid block (default: the first free block from 200000) |
| `--unit FILE`, `--unit-sha256 HEX` | `js-agent-crun.service`; an apply needs both |
| `--tmpfiles FILE`, `--tmpfiles-sha256 HEX` | `tmpfiles-crun.conf`; an apply needs both |
| `--server URL`, `--pool NAME` | written into the unit in place of its placeholders; an apply needs both |
| `--scratch-mount-unit NAME` | the mount unit of `/var/lib/js-agent/scratch`, a file in `/etc/systemd/system` (default `var-lib-js\x2dagent-scratch.mount`) |
| `--skip-scratch-quota` | leave the scratch mount as it is, for a host whose scratch is not a mount unit |
| `--root DIR` | run against the tree `DIR` instead of `/`; for tests only. The programs it runs still act on the host, and the script's root check (`id -u`) still wants euid 0 for `--apply`, while the guard against acting on the host wants the real uid (`id -ur`) not to be 0: so `--apply` with `--root` can work only with euid 0 and ruid not 0, which no ordinary login has |

The script reads no environment variable. Exit codes: 0 done, or a plan; 1 a step failed; 2 refused or
bad usage, with nothing changed; 3 nothing to do.

Every refusal is found before the first write: a dry run shows the same refusals an apply would.
A rerun on a host that is set up changes nothing and exits 3, so it is safe to run again after an
interruption or to check a host.

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
  RequiredBy,RequisiteOf,BoundBy,ConsistsOf`). A custom target counts; only `local-fs.target` is
  left out;
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

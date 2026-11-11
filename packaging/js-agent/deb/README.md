# js-agent as a .deb (Linux)

`cargo xtask release deb` builds `js-agent_<version>_amd64.deb` and `js-agent_<version>_arm64.deb`
from the files in this directory and the release's static `js-agent` binary
(`crates/xtask/src/release/deb.rs`, which compiles these texts in). The release publishes them to
the apt repository at `https://packages.komira.ai/apt` (pool `pool/jobscheduler/`), and
hostkeeper installs the stable suite's package on a host as the service `js-agent`: its native apt
installer installs the package at `activate`, switches
`/var/lib/hostkeeper/packages/js-agent/current` to the version directory, and runs the agent's
update hook (`/usr/bin/js-agent update-hook`) for every update.

## What the package holds

| file | installed at | |
|---|---|---|
| `js-agent` (the release binary) | `/usr/bin/js-agent` | static, byte for byte what the release builds |
| `js-agent-crun.service` | `/usr/lib/systemd/system/js-agent-crun.service` | the agent, run with crun; the one unit hostkeeper starts, stops and enables |
| `js-agent-crun-gpu.service` | `/usr/lib/systemd/system/js-agent-crun-gpu.service` | the oneshot that writes the GPU drop-in; pulled in by the agent's unit |
| `tmpfiles.conf` | `/usr/lib/tmpfiles.d/js-agent-crun.conf` | the directories of the unit |
| `sysusers.conf` | `/usr/lib/sysusers.d/js-agent.conf` | the job user `jsjob` the unit names with `--job-user`, which the postinst makes |
| `../js-agent.service` | `/usr/lib/systemd/system/js-agent.service` | the agent run through Docker or Podman, for a host that installs the package by hand; hostkeeper does not start it |
| `../tmpfiles.conf` | `/usr/lib/tmpfiles.d/js-agent.conf` | the directories of that unit |
| `../crun_host_setup.sh` | `/usr/share/js-agent/crun_host_setup.sh` | the host setup script, mode 0755 |
| `LICENSE` | `/usr/share/doc/js-agent/copyright` | |

The package has one maintainer script, `postinst`, which runs `systemd-sysusers js-agent.conf` at
`configure`: it makes the job user `jsjob` (a system account with a group of the same name, a uid
and gid from the system range, home `/nonexistent`, shell `/usr/sbin/nologin`) and keeps an
account of that name that exists already, as one the setup script or an operator made. Its one
`Depends`, `systemd | systemd-standalone-sysusers`, is what provides `systemd-sysusers`; every
host that runs the package's units has `systemd`, so it adds nothing to install there. Removing
or purging the package leaves the account. The package has no conffile and enables no unit: the
install changes nothing a running agent reads until hostkeeper restarts the unit.

## What the host provides

The agent's start check of `--runtime crun` (`js-agent-crun.service` exits 2 and
`journalctl -u js-agent-crun` lists each missing piece) needs more than the package carries. The
package makes the job user; the rest is the host's, and is placed before hostkeeper starts the
unit, by `crun_host_setup.sh` (`docs/runbooks/crun-host-setup.md`), by the machine image, or by
hand:

| piece the check names | what the host needs | why the package does not provide it |
|---|---|---|
| `crun`, `crun version` | the distribution's `crun` package, 1.14 or newer, at `/usr/bin/crun` | not a `Depends`: hostkeeper's install must find nothing to install but the package, and the program must be the distribution's file, which its AppArmor profile attaches to |
| `newuidmap`, `newgidmap` | the `uidmap` package | not a `Depends`, as for `crun` |
| `subuid range`, `subgid range` | a range of 65536 ids or more for `jsjob` in `/etc/subuid` and in `/etc/subgid`, overlapping no other range, uid or gid of the host: a line `jsjob:<first id>:65536` in each, which names the user and so can be placed before the package makes the account (after it, `usermod --add-subuids` and `--add-subgids` write the same lines) | the free block is the host's to choose: the setup script picks the first free one from 200000 |
| `apparmor profile` | where `kernel.apparmor_restrict_unprivileged_userns` is 1, the distribution's profile for `/usr/bin/crun` loaded (`apparmor`) | the profile is the distribution's |
| `scratch root`, `path permissions` | the scratch filesystem mounted at `/var/lib/js-agent/scratch` (the unit starts only on a mount point there), with project quotas (`prjquota`) | the disk is the host's |
| `cgroup v2 delegation` | cgroup v2 and systemd 254 or newer | the unit asks for it (`Delegate=yes`, `DelegateSubgroup=supervisor`) |

The state root and the other directories are the package's tmpfiles file's, made at each start of
the unit.

`js-agent-crun.service` is `packaging/js-agent/js-agent-crun.service`, the unit of the setup script
(`crun_host_setup.sh`), with one difference of substance: the server and pool are not in the unit
but read from a flags file. The setup script, its unit and its machine image stay for hosts that
are not on hostkeeper yet. `crates/xtask/tests/js_agent_deb_units.rs` pins the units against the
farm unit and the hook's params.

The unit is in `/usr/lib/systemd/system`, so an operator's override is a drop-in in
`/etc/systemd/system/js-agent-crun.service.d` (`systemctl edit js-agent-crun`), and the GPU oneshot
reads that directory for the `ExecStart=` it extends (`--dropin-dir`).

## What the operator places

A package is public and the same on every host, so it can place neither a secret nor a host's
own settings. These three files are the operator's (or the machine image's), root's, in
`/etc/js-agent`, which the package's tmpfiles file makes mode 0700:

| file | holds | mode |
|---|---|---|
| `/etc/js-agent/token` | the join token, one line: the pool's token, or the one-time token minted for this instance (`js join-token mint`) | root:root 0600 |
| `/etc/js-agent/flags` | the host's own agent flags, one per line (below) | root-owned, not writable by group or others |
| `/etc/js-agent/instance` | `PROVIDER:INSTANCE_ID`, the instance a one-time token is bound to (not secret) | root-owned |

The flags file is read by `js-agent --flags-file /etc/js-agent/flags`, which the unit passes, and
by the update hook. One argument word per line; empty lines and lines that start with `#` are
skipped:

```
--server=https://js.example.com:8443
--pool=farm
--instance-file=/etc/js-agent/instance
--host-id=node-5
--ca-cert=/etc/js-agent/ca.pem
```

`--server` and `--pool` are required; the others are optional. The agent refuses a flags file
that is not a regular file owned by root (or the user running the agent), is writable by group or
others, or holds a line that is not a flag.

The first install of a host defers (the hook reports `join_token_missing` and hostkeeper retries)
until `/etc/js-agent/token` is a regular file, owned by root, mode 0600 or 0400. Place the
files first, then let hostkeeper install `js-agent`.

## The hook manifest

hostkeeper's native entries carry the manifest in the order that sets the desired version, not in
the package: `cargo xtask release desired` writes it into the `targets` that
`.github/workflows/hostkeeper-desired.yml` hands to hostkeeper. Its shape, for each platform:

```
"manifest": {
  "package": "js-agent",
  "hook": {
    "command": ["/usr/bin/js-agent", "update-hook"],
    "timeoutSecs": {"prepare": "3900", "cutover": "4320", "verify": "615", "rollback": "8220", "cleanup": "120"},
    "verifyDeadlineSecs": "9000",
    "cleanupRetries": 3
  },
  "run": ["js-agent-crun.service"],
  "params": { ... },
  "minHostkeeper": "0.1.225",
  "installerBounds": {"stageSecs": "600", "activateSecs": "300", "removeSecs": "300"}
}
```

- `hook.command` is the program of every step. hostkeeper runs the update steps (prepare,
  cutover, verify, rollback, cleanup) as `/usr/bin/js-agent update-hook <step> [<dir>] --params
  <file>`.
- The bounds: `prepare` 3900 s (the step waits for the agent's running jobs to be cordoned and
  drained, up to 3600 s, plus a margin), `cutover` 4320 s (the stop of the agent, a drain of up to
  3600 s and 120 s to stop jobs, the wait for the unit's cgroup to empty, and the start), `verify`
  615 s (the new agent runs and is registered), `rollback` 8220 s (the stop and start back to the
  previous release), `cleanup` 120 s. `verifyDeadlineSecs` is at least verify plus rollback
  (615 + 8220), so a failed verify can still be rolled back. Each bound is mirrored in `params`
  for the hook.
- There is no `uninstall` bound: hostkeeper refuses an uninstall order for this package.
- `params` is what the hook reads, as the `--params` file hostkeeper writes: the unit it
  restarts (`js-agent-crun.service`), its state directory, hostkeeper's `current` link, the flags
  file and token file, the agent's state root and runtime (`crun`), the cordon and drain bounds
  and the step bounds above. The keys are `js_agent::update_hook::params::KEYS`;
  the tests of `cargo xtask release desired` check that the manifest carries exactly those.

The hook reads the release directory hostkeeper passes it. A package installed by apt keeps its
files below `tree/` in that directory (`tree/usr/bin/js-agent`,
`tree/usr/lib/systemd/system/js-agent-crun.service`), and the hook reads that layout; a directory
without `tree/` is an archive release of the earlier format (`bin/js-agent`, `units/<unit>`),
which the first update of a host that ran one still compares the units with.

## Moving a host from the setup script's unit

hostkeeper's installer does not replace a file of the host's, so remove the setup script's copy of
`/etc/systemd/system/js-agent-crun.service`, of `/etc/systemd/system/js-agent-crun-gpu.service` and
of `/etc/tmpfiles.d/js-agent-crun.conf` first (they would override the package's units), and any
`/etc/systemd/system/js-agent-crun.service.d` override that sets `ExecStart=`. Move `--server` and `--pool` into `/etc/js-agent/flags`, then let hostkeeper install
`js-agent`. The setup script's `/opt/js-agent` release layout is not used by the package.

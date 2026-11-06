# js-agent as a hostkeeper package (Linux)

`cargo xtask release hostkeeper-package` builds `js-agent-<version>-linux-x86_64.tar.zst` and
`js-agent-<version>-linux-arm64.tar.zst` from the files in this directory and the release's
`js-agent` binary (`crates/xtask/src/release/hostkeeper_package.rs`).
`cargo xtask release hostkeeper-index` writes the release index that lists the three platforms'
packages (the Mac's is in `packaging/js-agent-macos/hostkeeper`). hostkeeper installs a package
under `/var/lib/hostkeeper/packages/js-agent/<version>/`, points
`/var/lib/hostkeeper/packages/js-agent/current` at it, links the files below into the system and
runs the agent's update hook for every update.

## What the package holds

| file | placed by hostkeeper at | |
|---|---|---|
| `bin/js-agent` | `/var/lib/hostkeeper/packages/js-agent/current/bin/js-agent` | the release binary, byte for byte |
| `units/js-agent-crun.service` | `/etc/systemd/system/js-agent-crun.service` | the agent, run with crun; the one unit hostkeeper starts, stops and enables |
| `units/js-agent-crun-gpu.service` | `/etc/systemd/system/js-agent-crun-gpu.service` | the oneshot that writes the GPU drop-in; pulled in by the agent's unit |
| `files/tmpfiles.conf` | `/etc/tmpfiles.d/js-agent-crun.conf` | the directories of the unit |
| `manifest.toml` | (read by hostkeeper) | the hook, its bounds, the run unit, the files and the hook's params; `manifest.toml.in` explains each key |

`js-agent-crun.service` is `packaging/js-agent/js-agent-crun.service`, the unit of the setup script
(`crun_host_setup.sh`), with one difference of substance: the program is run through hostkeeper's
`current` link, and the server and pool are not in the unit but read from a flags file. The setup
script, its unit and its machine image stay for hosts that are not on hostkeeper yet.
`crates/xtask/tests/hostkeeper_package` pins the units against the farm unit.

## What the operator places

A package is public and the same on every host, and hostkeeper links files into a tree every
user can read, so it can place neither a secret nor a host's own settings. These three files are
the operator's (or the machine image's), root's, in `/etc/js-agent`, which the package's
tmpfiles file makes mode 0700:

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
files first, then order hostkeeper to install `js-agent`.

## Trusting the package

hostkeeper installs only what a trusted workflow attested. The package is attested by
`.github/workflows/release.yml` of `komira-ai/jobscheduler` on `refs/heads/main`. The entry a
channel needs for the service `js-agent`:

| key | value |
|---|---|
| service | `js-agent` |
| repository | `komira-ai/jobscheduler` |
| workflow | `.github/workflows/release.yml` |
| refs | `refs/heads/main` |

## Moving a host from the setup script's unit

hostkeeper refuses to link a path where a file of the host's is (`path_conflict`), so remove the
setup script's copy of `/etc/systemd/system/js-agent-crun.service`, of
`/etc/systemd/system/js-agent-crun-gpu.service` and of `/etc/tmpfiles.d/js-agent-crun.conf` first,
and any `/etc/systemd/system/js-agent-crun.service.d` override that sets `ExecStart=`. Move
`--server` and `--pool` into `/etc/js-agent/flags`, then order the install of `js-agent`. The
setup script's `/opt/js-agent` release layout is not used by the package.

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
| `sysusers.conf` | `/usr/lib/sysusers.d/js-agent.conf` | the job user `jsjob` the unit names with `--job-user`, which the postinst makes (and gives its subordinate ranges) |
| `../js-agent.service` | `/usr/lib/systemd/system/js-agent.service` | the agent run through Docker or Podman, for a host that installs the package by hand; hostkeeper does not start it |
| `../tmpfiles.conf` | `/usr/lib/tmpfiles.d/js-agent.conf` | the directories of that unit |
| `../crun_host_setup.sh` | `/usr/share/js-agent/crun_host_setup.sh` | the host setup script, mode 0755 |
| `LICENSE` | `/usr/share/doc/js-agent/copyright` | |

The package has one maintainer script, `postinst`, which acts at `configure` (and at the abort
steps that put a configured package back):

1. `systemd-sysusers js-agent.conf` makes the job user `jsjob` (a system account with a group of
   the same name, a uid and gid from the system range, home `/nonexistent`, shell
   `/usr/sbin/nologin`) and keeps an account of that name that exists already, as one the setup
   script or an operator made.
2. It gives `jsjob` a subordinate range in each of `/etc/subuid` and `/etc/subgid` that has none.
   A file that has one is left as it is: a line naming the user (or its uid) with a first id
   above 0 and 65536 ids or more that end below 2^32, the range the agent reads, as the
   `jsjob:296608:65536` a host set up by the setup script has. The new range is
   `jsjob:<first>:65536`, the same in both files, where `<first>` is the setup script's choice:
   the first id from 200000 that starts 65536 ids overlapping no range of either file and holding
   no uid of `/etc/passwd` and no gid of `/etc/group`. When no such block ends below 2^32 the
   postinst writes nothing, warns on standard error and the install goes on; the agent's start
   check then names the missing range.

Its one
`Depends`, `systemd | systemd-standalone-sysusers`, is what provides `systemd-sysusers`; every
host that runs the package's units has `systemd`, so it adds nothing to install there. Removing
or purging the package leaves the account and its ranges. The package has no conffile and enables no unit: the
install changes nothing a running agent reads until hostkeeper restarts the unit.

## What the host provides

The agent's start check of the crun runtime (`js-agent-crun.service` exits 2 and
`journalctl -u js-agent-crun` lists each missing piece) needs more than the package carries. The
package makes the job user and its subordinate ranges; the rest is the host's, and is placed before hostkeeper starts the
unit, by `crun_host_setup.sh` (`docs/runbooks/crun-host-setup.md`), by the machine image, or by
hand:

| piece the check names | what the host needs | why the package does not provide it |
|---|---|---|
| `crun`, `crun version` | the distribution's `crun` package, 1.14 or newer, at `/usr/bin/crun` | not a `Depends`: hostkeeper's install must find nothing to install but the package, and the program must be the distribution's file, which its AppArmor profile attaches to |
| `newuidmap`, `newgidmap` | the `uidmap` package | not a `Depends`, as for `crun` |
| `apparmor profile` | where `kernel.apparmor_restrict_unprivileged_userns` is 1, the distribution's profile for `/usr/bin/crun` loaded (`apparmor`) | the profile is the distribution's |
| `scratch root`, `path permissions` | the scratch filesystem mounted at `/var/lib/js-agent/scratch` (the unit starts only on a mount point there), XFS with project quotas (`prjquota`): each job's `/tmp` and `/var/tmp` is a directory there bounded by a project quota (`--job-tmp-gib`), and an agent whose scratch filesystem has no `prjquota` starts no job (each start is refused `quota-unavailable`, and the finding `quota-unhealthy` says why) | the disk is the host's |
| `cgroup v2 delegation` | cgroup v2 and systemd 254 or newer | the unit asks for it (`Delegate=yes`, `DelegateSubgroup=supervisor`) |

The state root and the other directories are the package's tmpfiles file's, made at each start of
the unit.

`js-agent-crun.service` is `packaging/js-agent/js-agent-crun.service`, the unit of the setup script
(`crun_host_setup.sh`), with two differences of substance: the server and pool are not in the unit
but read from a flags file, and the unit passes `--health-listen 127.0.0.1:7311`, the route
hostkeeper probes as the package's `healthUrl` (below). The setup script, its unit and its machine
image stay for hosts that are not on hostkeeper yet. `crates/xtask/tests/js_agent_deb_units.rs`
pins the units against the farm unit and the hook's params.

The unit is in `/usr/lib/systemd/system`, so an operator's override is a drop-in in
`/etc/systemd/system/js-agent-crun.service.d` (`systemctl edit js-agent-crun`), and the GPU oneshot
reads that directory for the `ExecStart=` it extends (`--dropin-dir`).

## What the operator places

A package is public and the same on every host, so it can place neither a secret nor a host's
own settings. These three files are the operator's (or the machine image's), root's, in
`/etc/js-agent`, which the package's tmpfiles file makes mode 0700:

| file | holds | mode |
|---|---|---|
| `/etc/js-agent/token` | the join token, one line: the pool's token, or the one-time token minted for this instance (`js join-token mint`). Not needed on a host whose flags file names `--host-cert` and `--host-key` (below) | root:root 0600 |
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

A host that belongs to a host group does not name its id here: a host-group config package puts
`--host-id-from-facts=/var/lib/hostkeeper/facts.json` in the flags file in place of `--host-id`
(the two together are refused). The agent registers under the `host_id` of that file, which
hostkeeper's daemon writes; the update hook reads the same file, so it finds the host under the
same id. The facts file must be a regular file owned by root (or the user running the agent),
not writable by group or others, at most 64 KiB, JSON with `"schema":1` and a `host_id` the hello
accepts; a file that is missing or refused stops the agent (it never falls back to the machine
id).

The first install of a host defers (the hook reports `join_token_missing` and hostkeeper retries)
until the host has a way to authenticate to the lease server. On a host that hostkeeper manages
that is the host's own certificate and key: hostkeeper keeps them at fixed paths, renews both in place before
the certificate expires, and the host group's config package names them in `/etc/js-agent/flags`
with `--host-cert PATH` and `--host-key PATH` (both or neither). The key is mode 0640, owned by root, in a group that the agent's account
belongs to; after following links it must be a regular file owned by root or by the user running
the agent, with no access for others and not writable by its group (0600, 0400, 0640 and 0440
pass). When the flags file names both, the hook does not look at the token file: it goes on when
the certificate file is there and the key passes that rule, and the agent presents the certificate
to the lease server (which needs `--agent-client-ca`) and sends no token (it neither reads
`--token-file` nor keeps a `host-credential`). Otherwise the install defers and the detail names the
file. A host whose flags file names neither flag, or cannot be read, or a node launched with a
one-time token, has no certificate: the install defers until `/etc/js-agent/token` is a regular
file, owned by root, mode 0600 or 0400. Place the files first, then let hostkeeper install
`js-agent`.

## The hook manifest

hostkeeper's native entries carry the manifest in the order that sets the desired version, not in
the package: `cargo xtask release desired` writes it into the `targets` that
`.github/workflows/hostkeeper-desired.yml` hands to hostkeeper. Its shape, for each platform:

```
"manifest": {
  "package": "js-agent",
  "hook": {
    "command": ["/usr/bin/js-agent", "update-hook"],
    "drain": "drain",
    "resume": "resume",
    "timeoutSecs": {"prepare": "3900", "cutover": "4320", "verify": "615", "rollback": "8220", "cleanup": "120"},
    "verifyDeadlineSecs": "9000",
    "cleanupRetries": 3
  },
  "run": ["js-agent-crun.service"],
  "healthUrl": "http://127.0.0.1:7311/healthz",
  "params": { ... },
  "minHostkeeper": "0.1.225",
  "installerBounds": {"stageSecs": "600", "activateSecs": "300", "removeSecs": "300"}
}
```

- `healthUrl` is the route the agent answers on at the address of `--health-listen` in the unit
  (a test holds the two equal). hostkeeper probes it once per round over plain HTTP to the
  loopback and reports the service unhealthy unless it answers `2xx`, which holds a rollout's bake.
  `GET /healthz` answers `200` and `placeable` while the agent holds a welcome of the lease server
  and neither the agent (its marker) nor the lease server (the `host_taint` of its last reply)
  holds a taint on the host, and `503` and `not placeable: <reason>` otherwise; `journalctl -u
  js-agent-crun` has one `health:` line for each change. The route has no authentication, so the
  agent refuses an address that is not a loopback one. The Mac package's entry has no
  `healthUrl`: its agent is started without the flag.
- `hook.command` is the program of every step. hostkeeper runs the update steps (prepare,
  cutover, verify, rollback, cleanup) as `/usr/bin/js-agent update-hook <step> [<dir>] --params
  <file>`.
- `hook.drain` and `hook.resume` name the two maintenance steps, run as `/usr/bin/js-agent
  update-hook drain|resume --params <file>` around a hostkeeper run that needs the host quiet.
  `drain` writes the hold file in the agent's state root, cordons the host and returns once it is
  idle; it has no bound unless the run gives a deadline, past which it puts the host back and
  exits 75 (`drain_deadline`). `resume` restarts the agent when the host's facts changed, removes
  the hold file and lifts the cordon; without a `drain` before it, it changes nothing. A
  hostkeeper coordinator older than komira-ai/hostkeeper#468 refuses a manifest with these fields,
  so that hostkeeper release is deployed first.
- The bounds: `prepare` 3900 s (the step waits for the agent's running jobs to be cordoned and
  drained, up to 3600 s, plus a margin), `cutover` 4320 s (the stop of the agent, a drain of up to
  3600 s and 120 s to stop jobs, the wait for the unit's cgroup to empty, and the start), `verify`
  615 s (the new agent runs and is registered), `rollback` 8220 s (the stop and start back to the
  previous release), `cleanup` 120 s. `verifyDeadlineSecs` is at least verify plus rollback
  (615 + 8220), so a failed verify can still be rolled back. Each bound is mirrored in `params`
  for the hook. They cover the drain, which an update still makes on a host that cannot hand its
  jobs over. One that can (a running agent whose `js-agent --version` prints `stops: drain
  handover`, the unit unchanged, no `--egress` in the flags file) is neither cordoned nor drained:
  `prepare` returns at once, and `cutover` writes the handover file in the state root and stops
  the unit, which takes seconds, the agent leaving its jobs running for the new agent to adopt
  (docs/runbooks/crun-host-setup.md, "Agent updates while jobs run"). `rollback` hands them back
  the same way.
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

## Config packages: `js-agent config-hook`

A host group's flags file and certificate come in a config package, `js-agent-config-<group>`,
which installs `/etc/js-agent/flags` and the `ca.pem` the flags name and runs no service of its
own. The agent reads its flags once, at its start, so the package's hook hands the new files
over by restarting the agent. Its manifest names

```
"hook": {
  "command": ["/usr/bin/js-agent", "config-hook"],
  "timeoutSecs": {"prepare": "60", "cutover": "420", "verify": "300", "rollback": "420", "cleanup": "60", "uninstall": "60"}
}
```

and hostkeeper runs `/usr/bin/js-agent config-hook <step> [<dir>] --params <file>`, with the same
exit statuses, last line and lock as the update hook above. The installer has put the new files in
place before `cutover`, and the previous version's back before `rollback`.

- `prepare` reads and checks the params, and deletes the record an earlier update left when its
  `cleanup` did not run, so the update it begins restarts the agent again.
- `cutover` checks that the flags file now in place parses as the agent's flags and names a host
  id (a facts file read included). If it does not, the step ends `bad_config` (reversible) with
  nothing restarted, and hostkeeper rolls the files back. Otherwise it notes the session the lease
  server shows for the host (none when the server does not know the host or shows it in no
  session); a server that cannot be reached or refuses the question defers the step
  (`lease_unreachable`, retried after 60 s) before anything is restarted. Then it stops the unit, waits for it to be down, starts it and waits
  for it to be active. The restart is not preceded by a cordon or a drain: running jobs are
  stopped as they are on any stop of the agent. A run after the restart was recorded does not
  restart again.
- `verify` waits until the unit is active and the lease server shows the host `live` in a session
  other than the noted one: `no_hello` when none comes in `verify_timeout_secs` less 15 s,
  `lease_unreachable` (class environment) when the server could not be asked, `start_failed` when
  the unit is down.
- `rollback` restarts the unit onto the previous files. With `/etc/js-agent/token` in place it also
  waits up to 60 s for the agent's hello, and tells in its detail when none came: a missing hello
  never fails a rollback. A `cutover` that restarted nothing leaves the unit alone.
- `cleanup` deletes the hook's record once `verify` succeeded (`not_verified`, reversible,
  before).
- `uninstall` does nothing to the service: the files go with the package, and the agent keeps
  running on what it read.

`params` is a flat table of exactly these keys (the updater's `updater.*` keys are ignored, any
other key is refused; paths are absolute and plain):

```
unit = "js-agent-crun.service"
flags_file = "/etc/js-agent/flags"
token_file = "/etc/js-agent/token"
state_root = "/var/lib/js-agent/state"
state_dir = "/var/lib/js-agent-config"
cutover_timeout_secs = 420
verify_timeout_secs = 300
rollback_timeout_secs = 420
```

`state_dir` is the hook's lock and record, root's, mode 0700 (the hook makes it). The bounds must
exceed what the steps wait for: `cutover` 180 s (the wait for the start and a margin) plus the
wait for the agent to stop, which is what is left of it, so 420 s allows 240 s; `verify` 15 s;
`rollback` 240 s plus its wait for the stop, so 420 s allows 180 s. The hook refuses shorter
values (`bad_params`). The agent's own stop takes up to 120 s to stop its jobs.

## Moving a host from the setup script's unit

hostkeeper's installer does not replace a file of the host's, so remove the setup script's copy of
`/etc/systemd/system/js-agent-crun.service`, of `/etc/systemd/system/js-agent-crun-gpu.service` and
of `/etc/tmpfiles.d/js-agent-crun.conf` first (they would override the package's units), and any
`/etc/systemd/system/js-agent-crun.service.d` override that sets `ExecStart=`. Move `--server` and `--pool` into `/etc/js-agent/flags`, then let hostkeeper install
`js-agent`. The setup script's `/opt/js-agent` release layout is not used by the package.

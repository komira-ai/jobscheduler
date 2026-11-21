# js-agent on a Mac

A signed installer package (`js-agent-<version>-macos-arm64.pkg`, identifier
`com.komira.js-agent`) that lays down the agent, its root helper and the pf tool
(`js-mac-provision`), and the plists of the three LaunchDaemons that run them. It is a hostkeeper `macos-pkg`
service: hostkeeper downloads it from the release's desired state, checks its digest, and
installs it. The package runs no script (no preinstall, no postinstall), so installing it
changes nothing but its files.

It is built by `.github/workflows/js-agent-macos-pkg.yml` and read back by
`cargo xtask pkg check-macos`; `cargo xtask pkg stage-macos` is the one place that lists
what the package holds (`crates/xtask/src/macpkg.rs`).

## What is installed

| path | mode |
|---|---|
| `/Library/Application Support/js-agent/bin/{js-agent,js-mac-session,js-mac-provision}` | 0755 |
| `/Library/Application Support/js-agent/LICENSE` | 0644 |
| `/Library/Application Support/js-agent/log/` | 0755, empty; launchd writes the daemons' logs here |
| `/Library/Application Support/js-agent/update/` | 0700, empty; the update hook's state directory |
| `/Library/LaunchDaemons/js-agent.plist` | 0644 |
| `/Library/LaunchDaemons/js-agent.session.plist` | 0644 |
| `/Library/LaunchDaemons/js-agent.pf-pool.plist` | 0644 |

Everything is owned by root:wheel. The programs are the package's own files; there is no
release directory and no link of the package's own. The release that hostkeeper keeps is
the package's tree under `/var/lib/hostkeeper/packages/js-agent/current`, which is what
the update hook's `release_link` names.

## The daemons

`js-agent.session` (the root helper) and `js-agent` are described by flags only
(`js-mac-provision agent-plist`, `js-mac-provision session-plist --daemon-requirement <req>`;
the goldens are in `crates/js-mac-provision/tests/golden`). Both plists are complete as
shipped: nothing in them is a placeholder. The third daemon, `js-agent.pf-pool`
(`js-mac-provision pf-pool-plist`), is described below.

- The agent's plist names `--flags-file /etc/js-agent/flags` and `--token-file
  /etc/js-agent/token`, and the fixed state root, scratch root and helper socket. The host's
  own values (`--server`, `--pool`, `--native-network` and the `--native-pf-*` flags) are in
  `/etc/js-agent/flags`, which a per-group configuration package installs. The plist carries
  no server and no pool.
- The helper's plist holds its caller to `cdhash H"<40 hex>"`, the code directory hash of
  the `js-agent` binary of the same package. The workflow reads the hash from the built
  binary on the Mac (the linker signs arm64 binaries ad hoc) and gives it to
  `session-plist --daemon-requirement`. The helper needs no flags file, and its socket group
  is `_jsagent`.

The flags file is one argument per line (`--flags-file` in `crates/js-agent/src/flags_file.rs`).
It gives the server, the pool and the native runtime, and is what a config package for a host
group of Macs (`js-agent-config-mac-build`, which also places `/etc/js-agent/ca.pem`) installs.
An example, with documentation addresses:

```
--server=https://leases.example:7300
--pool=mac
--ca-cert=/etc/js-agent/ca.pem
--host-id-from-facts=/var/lib/hostkeeper/facts.json
--native-network=on
--native-pf-uid-start=600
--native-pf-uid-count=100
--native-pf-endpoint=192.0.2.10:443
--native-pf-endpoint=192.0.2.11:443
```

Without the file the agent does not start.
The file must be a regular file owned by root, not writable by group or others.
`--host-id-from-facts` makes the host's id the `host_id` of hostkeeper's facts file, the name the
coordinator enrolled the Mac under, so one file serves every Mac of the group.

Neither runs at load. `KeepAlive` holds each while the flags file `/etc/js-agent/flags`
exists, so the daemons start when the host has its flags, and launchd starts one again after
every exit, at most once in 30 seconds (`ThrottleInterval`). A refusal is read from
`/Library/Application Support/js-agent/log/js-agent.err.log`.

`js-agent.pf-pool` keeps the pool's pf anchor loaded, which the agent checks at its start and
before every job with the network `on` (see Network). Its plist is fixed too
(`js-mac-provision pf-pool-plist`, golden `crates/js-mac-provision/tests/golden/pf-pool.plist`):
as root, at load and every 30 seconds (`RunAtLoad`, `StartInterval`), it runs
`js-mac-provision install --flags-file /etc/js-agent/flags`, which reads the anchor from the
agent's own `--native-network` and `--native-pf-*` flags, loads it unless it is in force already,
and does nothing while the flags file is absent or sets no `--native-network=on`. So the anchor
comes back within 30 seconds of anything that drops it (a reboot, an OS update that reloads
`/etc/pf.conf`), and a config package that changes the pf flags changes the anchor with them. Its
errors go to `/var/log/js-mac-provision.log`.

## What hostkeeper does

For the service `js-agent` of the release's desired state for the group, hostkeeper runs the
update hook's `prepare` from the staged package tree, then installs the package with the system
installer and points its `current` link at the release, then runs the hook's `cutover`, `verify`
and `cleanup` (`/Library/Application Support/js-agent/bin/js-agent update-hook`), with the
params of the manifest: the unit
`js-agent`, the helper unit `js-agent.session`, the pf job `js-agent.pf-pool`, the state directory above, the release link
under `/var/lib/hostkeeper/packages/js-agent`, the flags and token files, the state root, the
helper's socket group and the scratch root (the last two name the host setup `prepare` makes,
see "Not done yet").
On a first install `prepare` finds no state directory and none of the directories above it
(the package is not installed yet), and makes them: `js-agent` mode 0755 and `update` mode 0700,
root's. A Mac joins by hostkeeper's host certificate: the flags file names it (`--host-cert`,
`--host-key`, hostkeeper's copy under `/var/lib/hostkeeper/identity`), and then no token file is
needed. The hook cordons and drains the agent; on a Mac the cutover, and the rollback, then replace
the helper's daemon and the agent's from the release tree with `launchctl` while the agent
is stopped, the helper before the agent. Between the two, the cutover boots the pf job out, runs
the staged release's `js-mac-provision install --flags-file /etc/js-agent/flags` to its end (so
the anchor is in force before the agent's start checks it, rather than racing the job's first
run), and bootstraps the pf job from the staged release; the rollback leaves the pf job as it is.
hostkeeper lists the three labels in the manifest's `run` and enables them, and launchd loads
them at every boot. A rollback restores the previous release if the agent does not
come back. Nothing in the package does any of this at install time.

## Signing

The workflow has two jobs. `build` runs in the beta stage of the release (`release.yml` calls
the workflow on a push to main) and by hand, never on a pull request; it builds the package
unsigned, checks it and uploads it, and names no secret: no code of a branch can see a signing
key. `sign` runs only on main, in the environment `pkg-signing`. It takes the checked package
from `build`, imports the installer certificate into a temporary keychain, finds the
"Developer ID Installer" identity there by its SHA-1, signs with `productsign --timestamp`,
checks the signed package again and writes `SHA256SUMS` for it. It does not notarize. When the
environment or its secrets are not set up it prints a notice and skips every step after the
first, and the run succeeds.

`pkg-signing` is created by a repository administrator, with a rule that only main may
deploy to it and with these secrets as environment secrets (none is in the tree):

- `MACOS_INSTALLER_P12`: the "Developer ID Installer" certificate and key, a PKCS#12 file, base64.
- `MACOS_INSTALLER_P12_PASSWORD`: its password.

No certificate name and no team id is written in the tree: the identity is found in the
temporary keychain by its hash. Each secret is named only in the environment of the step
that needs it.

## Config packages: `js-agent config-hook`

A Mac's flags file and certificate come in a config package (`js-agent-config-<group>`, a `.pkg`
whose files are `/etc/js-agent/flags` and the certificate the flags name). Its hook restarts the
agent's daemon so that the agent reads the new flags; the steps and what they check are the Linux
package's (`packaging/js-agent/deb/README.md`, "Config packages"). The manifest names

```
"hook": {
  "command": ["/Library/Application Support/js-agent/bin/js-agent", "config-hook"],
  "timeoutSecs": {"prepare": "60", "cutover": "420", "verify": "300", "rollback": "420", "cleanup": "60", "uninstall": "60"}
}
```

with the params

```
unit = "js-agent"
flags_file = "/etc/js-agent/flags"
token_file = "/etc/js-agent/token"
state_root = "/var/db/js-agent/state"
state_dir = "/var/db/js-agent-config"
cutover_timeout_secs = 420
verify_timeout_secs = 300
rollback_timeout_secs = 420
```

`unit` is the daemon's launchd label. The restart is `bootout` (which returns when the agent has
exited, up to its 120 s `ExitTimeOut`) and `bootstrap`, held back like the update hook's while
`/etc/js-agent/flags` is absent. The unit port has run against a fake of `launchctl` only.

## Privacy grants (TCC)

The agent and the helper need no privacy grant (Full Disk Access, or any other the
Privacy & Security settings list), and nothing in this package or this repository gives
one. A grant given to either by hand, or by a profile, never reaches a job. macOS lets a
process use the grants of its *responsible* process, which is whoever started it, unless
the starter disclaims that responsibility as it starts it. Every process the helper
starts for a job (the job's program, and `launchctl` with the helper's `as-user` mode for
a GUI session), and every program the agent starts itself (its probes, the `sandbox-exec`
self-test, `pfctl`), is started with that responsibility disclaimed
(`crates/js-mac-session/src/disclaim.rs`; the design is in
`docs/design/spawn-disclaim.md`). A job's process therefore answers for itself, and the
processes it starts answer for it: none of them holds the agent's or the helper's grants.
What a job may do is then what its own program was granted. A grant that someone gave a
job's program itself (an app the job runs, say) is that program's, and the disclaim does
not take it away.

The hosted CI runner has no grant to give, so CI checks the disclaim and not a refused
read: on the macOS runner the agent's tests read back the responsible process of the
processes the helper's `run` and the agent start, and see that each is the process itself.
To check a host by hand that a grant does not reach a job: give `js-mac-session` Full
Disk Access in Privacy & Security, run a native job whose program is `/bin/ls` with the
argument `/Library/Application Support/com.apple.TCC`, and see it fail with `Operation not
permitted`. A refusal is also what a grant that did not take effect gives, so check the
grant the same way with a release from before the disclaim (one whose
`crates/js-mac-session/src/disclaim.rs` does not exist): there the same job answers for
the helper and lists the directory. Remove the grant afterwards. `sudo launchctl procinfo <pid>` of a running job's
process shows its `responsible pid` equal to its own pid.

## Network

A Mac has no network namespace, so a native job's network level maps onto the Seatbelt
profile and the pool's pf anchor (`crates/js-mac-provision`, its README has the rules). The
agent's ceiling is `--native-network`:

| Job level | `--native-network` value | What the job gets | Needs |
|---|---|---|---|
| `none` | `off` (the default) | the profile denies the network | nothing |
| `allow-list` | `on` | the profile allows the network; the anchor limits it to the listed endpoints | the anchor in force (the `--native-pf-*` flags), checked at start and before each `allow-list` job (`egress-unchecked`) |
| `open` | `open` | the profile allows the network; nothing filters it | nothing; a `--native-pf-*` flag beside it stops the agent at start |

`loopback` is a ceiling with no job level of its own: it admits a job with no network and a
native body that asks for `loopback`, which every run of the Mac shares. A job that asks for
more than the ceiling is refused at start (`network-above-ceiling`, before any user is made)
and is never run with less. Jobs with no network, loopback only or `open` never ask `pfctl`.
The hello reports the ceiling as `egress`: `none` for `off` and `loopback`, `allow-list` for
`on`, `open` for `open`, and no `egress-ports`. A bundle's `seatbelt.network_max` is not a
ceiling: it says what the bundle's tools need and lowers the profile of an admitted job to it.

Known limits:

- Declared ports are refused (`ports-not-supported`, before any user is made). The root test
  `crates/js-agent/tests/native_inbound_pf/`, built by the manual workflow
  `js-agent-native-inbound-pf-test` and run by an operator as root on a provisioned Mac, asks
  whether the anchor filters inbound traffic to a pool uid's listener; the refusal stands
  until it passes.
- `open` leaves the network unfiltered: the scheduler, the cloud metadata address and every
  other address the Mac reaches are reachable from the job.
- DNS through the system resolver (mDNSResponder, a unix socket) is outside the filter.
- pf matches `user` on TCP and UDP only, and loopback between pool users cannot be narrowed by
  the sender's uid.

## Not done yet

- The update hook's `launchctl` form (`bootout`, `bootstrap`, `kickstart -k`, `print`) has
  run against a fake of `launchctl` only, never against a real launchd: what `launchctl print`
  prints of a daemon that crashed (its `last exit code`), the time a `bootstrap` takes to start
  the daemon, and `bootout` waiting out the agent's `ExitTimeOut` are read from the manual and
  from what the fake says, not from a Mac. `kickstart -k` runs a daemon whatever its `KeepAlive`
  condition says, so the port refuses it while `/etc/js-agent/flags` is absent.
- The package is not notarized.
- The pf job and the cutover's `js-mac-provision install --flags-file` have run against a fake
  `pfctl` and a fake `launchctl` only, not on a Mac.
- The config hook (`js-agent config-hook`) restarts the agent without loading the anchor first:
  a config package that changes the `--native-pf-*` flags of a running Mac leaves the new anchor
  to the pf job's next run (within 30 seconds), and an agent that starts before it refuses to
  start (`--native-network: on needs the pf anchor of the pool in force`) until launchd starts it
  again.
- The package carries no host setup. The update hook's `prepare` makes it on the Mac, as
  root, from its params (`socket_group`, `scratch_root`): the helper's socket group
  `_jsagent`, the agent's state root `/var/db/js-agent/state` and a scratch root
  `/var/db/js-agent/scratch` on an APFS volume of its own (`js-agent-scratch`), as the agent
  requires. Nothing names the volume in `/etc/fstab`, which macOS lets only a process with Full
  Disk Access write: the agent mounts the volume on its scratch root at each start
  (`diskutil mount nobrowse -mountPoint`), so a reboot needs nothing more. `/etc/js-agent/flags`
  comes from the group's config package. The setup and the agent's mount have run against
  fakes of `dscl`, `dseditgroup` and `diskutil` only.

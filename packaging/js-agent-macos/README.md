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
| `/Library/Application Support/js-agent/bin/{js-agent,js-mac-session,js-mac-provision}` | 0755, signed (see Signing) |
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
  /etc/js-agent/token`, and the fixed state root and scratch root. The host's
  own values (`--server`, `--pool`, `--native-network`, `--native-accounts`,
  `--native-account-range`, `--native-account-ready` and the `--native-pf-*` flags) are in
  `/etc/js-agent/flags`, which a per-group configuration package installs. The plist carries
  no server and no pool.
- The helper's plist holds its caller to `cdhash H"<40 hex>"`, the code directory hash of
  the `js-agent` binary of the same package. The workflow reads the hash from the signed
  binary that goes into the package (`sign-code` signed it with the Developer ID Application
  identity; a build that no job signs, such as the first-install test's, reads the hash the
  linker's ad hoc signature gives) and gives it to `session-plist --daemon-requirement`. The helper needs no flags file, and its socket group
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
--native-accounts=/var/db/js-agent/accounts
--native-account-range=700-799
--native-account-ready=1
--native-network=on
--native-pf-uid-start=600
--native-pf-uid-count=100
--native-pf-endpoint=192.0.2.10:443
--native-pf-endpoint=192.0.2.11:443
```

Without the file the agent does not start, nor without `--native-accounts` (or `--no-jobs`, for
an agent that runs no job).
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

The helper's pool users (`kbf-lease-pool-<n>`, hidden) are made once, by the helper's boot reset,
and stay for good: the system refuses root the deletion of a local user record, of an existing
home and of the per-user folders it placed (all can be emptied), so the helper never deletes one
and resets a user in place when a lease ends. A first install on a Mac that already has them (an
earlier install left them, even with `/var/db/js-mac-session` gone) adopts each that is the pool's
own (its name, uid, hidden flag and home `/Users/kbf-lease-pool-<n>` match), resets it and
makes the slot free; a record on a pool uid that is not the pool's is refused and the slot is
left unmade, which the helper's log and `pool-status` name. Removing the package leaves the users.

A job writes only in its run directory, `<home>/exec`: the profile grants nothing else in the
account's home, and the job's `HOME` and `CFFIXED_USER_HOME` are `<home>/exec/.home` and its `TMPDIR`
is `<home>/exec/.tmp`, so a tool that keeps preferences or caches writes there and the account's
real home stays empty (no `~/Library` is made in it). The helper sets `HOME` to the account's home
whatever the agent asks, so the agent runs the program as `/usr/bin/env HOME=<run home> <program>`
inside the profile. This matters because macOS guards `~/Library/Preferences` of an account's real
home even against root without Full Disk Access (the profile below grants it to the agent, not the helper, and the pool reset of this release does not rely on it):
a folder a job made there could not be removed by the reset. A reset keeps an empty folder the
system refuses root the removal of (reported in the sweep's `kept`) and does not fail the slot over
it; a folder that is refused and not empty still fails the reset, naming it. Such an empty
`~/Library/Preferences` is what a job of an agent before this change left in a slot's home. Only a
process with Full Disk Access (a person at a Terminal that was granted it, as an act on that Mac)
could remove it, or nothing; the pool tolerates it as it is, and a job can no longer make one.

## Signing

The workflow has four jobs. `build` runs in the beta stage of the release (`release.yml` calls
the workflow on a push to main) and by hand, never on a pull request; it builds the package
unsigned (the steps are the local action `.github/actions/js-agent-macos-pkg`), checks it and
uploads it with the three programs it built, and names no secret: no code of a branch can see a
signing key. `build-update` runs
beside it and builds a second package with the same action, of the next patch version
(`.github/actions/js-agent-next-version`), as the artifact `js-agent-macos-pkg-update-unsigned`
for a day: the first install updates the agent to it, and nothing signs it or releases it.
`first-install` runs on a manual run alone, on any branch, and calls
`js-agent-macos-first-install.yml` with the version typed: it installs the unsigned package
`build` made on the runner, the way hostkeeper does, runs the update hook's prepare,
cutover, verify and cleanup as root with launchd loading the jobs, rolls a config package over
the agent and updates the agent to the package `build-update` made (the release runs it as its own
beta job, `beta-macos-first-install`). Three more jobs run in the release's call (on a push to
main) and in a signing run, in this order. A signing run is a manual run with `sign` set, on a
branch of this repository (`gh workflow run js-agent-macos-pkg.yml --ref <branch> -f version=X -f
sign=true`): it signs and checks the programs and the package exactly as the release does and
publishes nothing (the signed files stay artifacts of the run), so a change to the signing is
proved before it merges. On a tag or in a fork's copy it fails by name at the first step of
`build`. A manual run without `sign` signs nothing.
`sign-code` runs in the environment `pkg-signing`: it takes the three programs from `build`,
checks them against `build`'s sums, imports the application certificate into a temporary
keychain for `/usr/bin/codesign` alone, imports Apple's Developer ID G2 intermediate (fetched
over HTTPS and held to its SHA-256; a hosted runner that holds it already makes `security import`
answer `The specified item already exists in the keychain.`, and that answer alone, exactly,
passes), finds the identity `Developer ID Application: Komira AI, Inc. (U7RTX4XD3X)` among the
keychain's valid code-signing identities (`find-identity -v` lists it only when its chain builds
to Apple's root), fails when it expires within 30 days, and signs each program with
`codesign --options runtime --timestamp` under the identifier of the table in
`crates/xtask/src/release/mac_programs.rs` (`com.komira.js-agent`, `com.komira.js-agent.session`,
`com.komira.js-agent.provision`). It runs nothing built from the repository, and it fails,
naming the secret, when one is not set. `package` has no secret: it makes the package with the
local action from the signed programs instead of building them, checking each staged program
first with `cargo xtask release check-code-signature <binary> --program <name>`, which runs
`codesign --verify --strict` and reads the signature back (an ad hoc signature, another
identifier or team, an authority chain other than the team's Developer ID chain, no timestamp,
no hardened runtime or another designated requirement is refused). `sign` runs in the same
environment: it takes the checked package from `package`, imports the installer certificate into
a temporary keychain, finds the "Developer ID Installer" identity there by its SHA-1, signs with
`productsign --timestamp`, checks the signed package again and writes `SHA256SUMS` for it. It
does not notarize. When the environment or the installer certificate's secrets are not set `sign`
prints a notice and skips every step after the first, and the run succeeds.

`pkg-signing` is created by a repository administrator with these secrets as environment
secrets (none is in the tree). It admits every branch of this repository, which the signing run
needs; anyone who can push a branch can already run a workflow of that branch in it:

- `MACOS_INSTALLER_P12`: the "Developer ID Installer" certificate and key, a PKCS#12 file, base64.
- `MACOS_INSTALLER_P12_PASSWORD`: its password.
- `MACOS_APPLICATION_P12`: the "Developer ID Application" certificate and key, a PKCS#12 file,
  base64.
- `MACOS_APPLICATION_P12_PASSWORD`: its password.

The installer identity is found in the temporary keychain by its SHA-1. The application
identity is named in `sign-code` and in the table of `mac_programs.rs` (its name and the team id
`U7RTX4XD3X` are in every program's signature, so they are public), and a test holds the two
together. Each secret is named only in the environment of the steps that use it.

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
`/etc/js-agent/flags` is absent. The hook works whichever came first, the agent's package or the
config package: a label that is not loaded is bootstrapped, one that is loaded is booted out
first. When `bootstrap` or `kickstart -k` fails, the start reads `launchctl print` again and
makes the other command once if launchd now calls for it (a `bootstrap` refused with exit 5
because the label was loaded meanwhile becomes a `kickstart -k` of an idle service, or nothing
for a running one), as the update hook's start does (`crates/js-update-hook/src/launch_units.rs`,
"When launchd refuses the start"). The unit port has run against a fake of `launchctl` only.

## Privacy grants (TCC)

The agent `js-agent` is granted Full Disk Access (the privacy service `SystemPolicyAllFiles`), and
nothing else is granted anything. The grant is given by the profile
`mdm/js-agent-full-disk-access.mobileconfig` of this directory, which the owner installs through
device management (see `mdm/README.md`): one entry, for the path
`/Library/Application Support/js-agent/bin/js-agent` (where the package lays the program and
updates replace it in place) and the program's designated requirement, which names the identifier
`com.komira.js-agent`, Apple's Developer ID chain and the team and no hash, so every release of
the program meets it. That is why the release signs the programs (see Signing), and `cargo test -p
xtask` holds the profile to the table the signing uses. The root helper `js-mac-session` and
`js-mac-provision` need no grant. A native job and its supervisor are started without the
disclaim: they keep the responsibility, so they answer for `js-agent` and hold what `js-agent`
holds, Full Disk Access included. macOS lets a process use the grants of its *responsible*
process, which is whoever started it, unless the starter disclaims that responsibility as it
starts it. Every process between `js-agent` and the job is started so, and a job's own children
(compilers, shells, `buck2`) follow their parent and answer for the agent too. The reason is
the build trees: tierstore serves them over a network file system (NFS) loopback mount, and a
job started disclaimed is judged by its own code signature and is refused every read of that
mount, Apple's own tools (`ls`, `cat`, `python3`, compilers) included. Measured on a Mac
(macOS 27): disclaimed, the reads fail with `Operation not permitted` and the system log shows
the network-volumes privacy service denied; started as ordinary children of the signed,
profile-granted `js-agent`, the same reads as the same pool user succeed.

What keeps a job from using the rest of the grant is the slot user it runs as (Unix
permissions) and the Seatbelt profile applied in the job's child before its program runs.
Measured on a Mac, the prototype's profile does not keep such a job out of three places only
the privacy controls protect: the system privacy database's directory, the database, and
`/Library/Preferences/com.apple.TimeMachine.plist`. How that is closed is not decided
(`docs/design/spawn-disclaim.md`, "The fence, measured").

Every other program `js-agent` starts stays disclaimed, which is the default of
`js_mac_session::disclaim::Program` (`crates/js-mac-session/src/disclaim.rs`; the design is in
`docs/design/spawn-disclaim.md`): its probes (`sw_vers`, `sysctl`), `pfctl`, `launchctl`,
`ioreg`, and the agent re-executing itself in place.
Not decided: the VM launcher (`js-vmm lease`), which stays disclaimed for now, and the
graphical-session start (`launchctl asuser`), which launchd starts in the user's session and
which is not measured and stays disclaimed. After an unattended restart a Mac has no logged-in
user, so a start that needs a user's graphical session cannot happen until someone logs in.

The agent starts every native job through its supervisor, whose starts keep the
responsibility (`js-agent __supervise-native`, `crates/js-agent/src/mac_supervisor`), so the
job answers for the agent and holds its grant; the slot account and the Seatbelt profile are
what fence it. A call that keeps the responsibility (`Program::keep_responsibility`) is
allowed only in the files the `KEEPERS` constant of `crates/xtask/src/spawn_sites.rs` lists: the
supervisor's `launch.rs` (the agent's start of the supervisor) and `run.rs` (its start of the
job).

The hosted CI runner has no grant to give, so CI checks the starts and not a refused read: on
the macOS runner the agent's tests read back the responsible process of the processes the
helper's `run` and the agent start, and see that each disclaimed one is the process itself.
To see by hand whose grants a process uses, run a native job and run `sudo launchctl procinfo
<pid>` on its process. Today the helper starts the job disclaimed, so the `responsible pid`
equals the job's own pid. Once the supervisor starts jobs, the `responsible pid` is the agent's
pid. To see the effect, run a native job whose program is `/bin/ls` with the argument
`/Library/Application Support/com.apple.TCC`: it fails with `Operation not permitted` today,
and once the supervisor starts jobs it lists the directory unless the Seatbelt profile refuses
the read, which the prototype's profile, measured, does not.

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

- The update hook's unit tests run its `launchctl` form (`bootout`, `bootstrap`, `kickstart -k`,
  `print`) against a fake of `launchctl`; the real launchd is reached by the first-install
  test (`mac_first_install`, run by `js-agent-macos-first-install.yml` on a hosted runner), which
  covers a first install (a failed attempt and the next order), a config package rolled over the
  running agent (its `config-hook`, with its state directory under `/var`, a link) and an update
  of the running agent to the next version, each to `Succeeded`. It covers no rollback: what
  `launchctl print` prints of a daemon that crashed (its `last exit code`) and `bootout` waiting
  out the agent's `ExitTimeOut` are read from the manual and from what the fake says. In that test
  the config package carries a launcher at `/etc/js-agent/config-hook` that runs the installed
  `js-agent config-hook`, because hostkeeper at the pinned revision runs a hook from the
  package's own sealed tree only; the manifest above names the installed agent instead. `kickstart -k` runs a daemon whatever its `KeepAlive`
  condition says, so the port refuses it while `/etc/js-agent/flags` is absent.
- The package is not notarized.
- The pf job and the cutover's `js-mac-provision install --flags-file` have run against a fake
  `pfctl` and a fake `launchctl` in their unit tests; the first-install run is the only run of
  them on a Mac.
- The config hook (`js-agent config-hook`) restarts the agent without loading the anchor first:
  a config package that changes the `--native-pf-*` flags of a running Mac leaves the new anchor
  to the pf job's next run (within 30 seconds), and an agent that starts before it refuses to
  start (`--native-network: on needs the pf anchor of the pool in force`) until launchd starts it
  again.
- The package carries no host setup. The update hook's `prepare` makes it on the Mac, as
  root, from its params (`socket_group`, `scratch_root`): the helper's socket group
  `_jsagent`, the agent's state root `/var/db/js-agent/state` and a scratch root
  `/var/db/js-agent/scratch/runs` on an APFS volume of its own (`js-agent-scratch`), as the agent
  requires. The volume is mounted on `/var/db/js-agent/scratch` (root's, 0711), the scratch root's
  parent, so that the bundle store `/var/db/js-agent/scratch/bundles` (made by the agent at its
  start, root's, 0711) lies beside the scratch root on the same volume and a lease user can walk
  down to it. Nothing names the volume in `/etc/fstab`, which macOS lets only a process with Full
  Disk Access write: the agent mounts the volume on that directory at each start
  (`diskutil mount nobrowse -mountPoint`), so a reboot needs nothing more. `/etc/js-agent/flags`
  comes from the group's config package. The setup and the agent's mount have run against
  fakes of `dscl`, `dseditgroup` and `diskutil` only.
- No Mac with the package installed has been restarted yet. What a restart or a macOS update
  needs, and the drill that proves a restart on one Mac, are in
  [docs/runbooks/mac-restart-and-update.md](../../docs/runbooks/mac-restart-and-update.md).

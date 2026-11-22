# js-agent on a Mac, installed by hostkeeper

How js-agent reaches a Mac of a hostkeeper host group (such as `mac-build`) the way it reaches a Linux build node: a
release of this repository carries a signed macOS package, hostkeeper's coordinator orders it onto the group's Macs,
and the agent registers with the lease server and offers native jobs in `sandbox-exec` pool mode. It says what the
package holds, what hostkeeper requires, how a release reaches the Macs, and which steps are outside this repository.

## Where things stand

- `.github/workflows/js-agent-macos-pkg.yml` builds `js-agent-<version>-macos-arm64.pkg` on a hosted Apple silicon
  runner and signs it. Its `build` job stages the payload, reads the cdhash of the `js-agent` binary, builds the
  component with `pkgbuild` (no scripts) and checks it with `cargo xtask pkg check-macos`. Its `sign` job runs on main,
  by hand or in the release's call (`sign: true`), in the environment `pkg-signing`; without that environment's secrets
  it skips with a notice.
- `release.yml` calls it as `beta-macos-pkg` with the release's version. Its job `pkg-upload` puts the signed package
  in the package repository once `publish` passed and `beta-macos-pkg` has ended (it waits for no beta test), before
  the release's `desired` starts `hostkeeper-desired.yml`.
- `cargo xtask release desired --pkg-url` adds a `macos-arm64` entry to the release's targets when the package is in the
  package repository, and `hostkeeper-desired.yml` sets it for the host group `mac-build`.
- The agent reports the capabilities `os` and `arch` as detected (`crates/js-agent/src/host.rs`, `capabilities`):
  `os=macos`, `arch=arm64` on an Apple silicon Mac, and `os_version` as the major version of its `ProductVersion`
  (`crates/js-agent/src/os_version.rs`; bundle admission still reads the host's version locally,
  `crates/js-agent/src/bundle/host.rs`).
- kbf's executor asks every job for `os=linux`, and kbf's design leaves macOS out of scope until a macOS design exists:
  the Macs keep `kbf-daemon` with the native driver (kbf repository, `docs/design/actions-as-jobs.md`, section 14).
  A Mac agent that registers is therefore offered no kbf action until kbf asks for `os=macos`. This document does not
  change that; it makes the agent installable for the day kbf does (`kbf-on-js-agent.md`, D3).

## What hostkeeper requires of a Mac package

From hostkeeper's `macos-pkg` installer (hostkeeper repository, `docs/design/packages-repositories.md`, section 4.7,
and `crates/hostkeeper-install/src/macos_pkg/`):

1. `pkgutil --check-signature` passes and the leaf certificate's SHA-256 is one of the one to four signers in the
   Mac's `/etc/hostkeeper/macos-pkg.toml`, which the daemon reads at its start. Notarization is not required.
2. A component package, or a product archive with one component, selected by default, whose `Distribution` has no
   `script`, `installation-check` or `volume-check` element. The component's `PackageInfo` has `identifier` equal to
   the manifest's `package`, `version` equal to the version ordered, `install-location` `/`, and no `bundle`.
3. The `Bom` lists exactly the payload; every entry is owned by root, of group wheel or admin, not writable by group or
   others, with no setuid, setgid or sticky bit.
4. Each `run` label is the service's name or the name, a dot and more (`js-agent`, `js-agent.session`), and has a
   plist in the payload whose `Label` is that label and whose program is a file of the payload. The observer hashes
   the program of the first `run` label.
5. The design places a service's payload under `/Library/Application Support/<service>/` and one
   `/Library/LaunchDaemons/<label>.plist` per `run` label. The built `stage` does not check the prefix; this package
   keeps to it.

hostkeeper keeps each version in its own sealed directory, `launchctl enable`s each `run` label at `activate`, checks
the installed files for drift against the sealed copy, and rolls back by installing the previous version's kept
package. With a hook program in the manifest, the hook's `cutover` starts and stops the labels (hostkeeper's built-in
start applies only to an entry without one).

The coordinator resolves a release's `macos-arm64` entry in the named-version form (`installer: macos-pkg`,
`repository`, `packageVersion`, `digest`, `bytes`, `manifest`) through the repository's descriptor, whose
`formats.macos-pkg` gives the path and platforms, to
`<base>/<macos-pkg path>/<directory>/<version>/js-agent-<version>-macos-arm64.pkg`, and streams it to the Mac; the Mac
needs no store of its own. The directory defaults to the service's name, so js-agent, which is published under
`jobscheduler`, needs one `packages` entry in the descriptor that maps `js-agent` to the directory `jobscheduler`; the
release writes it (`channels descriptor write`, below). An entry that does not resolve leaves the Mac `no_build_for_host` and touches no Linux host.

## The package

`js-agent-<version>-macos-arm64.pkg`, identifier `com.komira.js-agent` (`crates/xtask/src/macpkg.rs`, `IDENTIFIER`),
version the release's, a component package built by `pkgbuild` and wrapped by `productbuild` with that one component.
It has no `preinstall` or `postinstall` script and its `Distribution` has no script element. Everything is root's,
group wheel; directories are 0755 unless noted.

| path | mode | |
|---|---|---|
| `/Library/Application Support/js-agent/bin/js-agent` | 0755 | the agent |
| `/Library/Application Support/js-agent/bin/js-mac-session` | 0755 | the root helper |
| `/Library/Application Support/js-agent/bin/js-mac-provision` | 0755 | the plist generator and the pf anchor's loader (the agent runs its install itself) |
| `/Library/Application Support/js-agent/LICENSE` | 0644 | |
| `/Library/Application Support/js-agent/log/` | dir | empty; the daemons' logs |
| `/Library/Application Support/js-agent/update/` | dir 0700 | empty; the update hook's `state_dir` |
| `/Library/LaunchDaemons/js-agent.plist` | 0644 | label `js-agent`: the agent |
| `/Library/LaunchDaemons/js-agent.session.plist` | 0644 | label `js-agent.session`: the root helper |

The parent directories `Library`, `Library/Application Support`, `Library/LaunchDaemons`,
`Library/Application Support/js-agent` and its `bin` are entries of the payload too. `cargo xtask pkg stage-macos
--version V --bin-dir D --license L --agent-plist A --session-plist S --out O` writes `O/root/...` and no
`O/scripts`; `cargo xtask pkg check-macos --pkg P --version V` refuses anything but exactly this payload (modes, root
as owner), any script, and another identifier or version.

- The labels are `js-agent` and `js-agent.session` (`crates/js-mac-provision/src/daemon.rs`:
  `AGENT_LABEL`, `SESSION_LABEL`; the plist goldens; the update hook's launchd tests). The `js-mac-provision` verbs
  of this package that print a plist are `agent-plist`, which takes no flag and prints the fixed agent plist, and
  `session-plist --daemon-requirement <REQ>`; `pf-pool-plist` prints the plist of the pf job an earlier release shipped
  and stays for a host on that release; of its pf anchor verbs, `install` also takes `--flags-file`.
  Its `postinstall` verb is gone, with its link and its tests.
- The agent plist is fixed text, checked for drift, never edited on a host. Its program arguments are
  `/Library/Application Support/js-agent/bin/js-agent --flags-file /etc/js-agent/flags --token-file
  /etc/js-agent/token --state-root /var/db/js-agent/state --scratch-root /var/db/js-agent/scratch/runs
  --native-helper-socket /var/run/js-mac-session/socket`; it runs as root, with `RunAtLoad` false, `ThrottleInterval`
  30, `ExitTimeOut` 120 and its logs in `.../js-agent/log/js-agent.log` and `js-agent.err.log`. `KeepAlive` is
  conditional on the flags file `/etc/js-agent/flags` (`PathState`), not on the token: a Mac joins by hostkeeper's
  host certificate, which the flags file names (`--host-cert`, `--host-key`), and has no token; the agent then reads no
  `--token-file`. It names no `--server` and no `--pool`: a host's own values (the
  lease server and its CA, the pool, `--native-network` and its pf flags) are in `/etc/js-agent/flags` and
  `/etc/js-agent/ca.pem`, which this package does not list. They are the group's: a config package per service and
  host group (`js-agent-config-mac-build`, listed in the group after `js-agent`) installs them, as
  `js-agent-config-all-in-one` installs `/etc/js-agent/{flags,ca.pem}` on the Linux nodes, so no operator edits them on
  a host. js-agent reads `--flags-file` already (`crates/js-agent/src/flags_file.rs`: a regular file, root's or the
  agent's, not writable by group or others, one word per line).
- The helper's plist is fixed text too, except for one value that the workflow bakes in when it builds: its program
  arguments are `/Library/Application Support/js-agent/bin/js-mac-session --socket /var/run/js-mac-session/socket
  --socket-group _jsagent --state-dir /var/db/js-mac-session --uid-range 600-699 --homes /Users --daemon-requirement
  <REQ>`, with `ExitTimeOut` 20 and its logs in `.../js-agent/log/js-mac-session.log` and `js-mac-session.err.log`.
  `<REQ>` is `cdhash H"<40 hex>"`, the cdhash of the `js-agent` binary of this same package, which the workflow reads on
  the macOS runner with `codesign --display --verbose=4` (the linker signs an arm64 binary ad hoc, so every binary has
  one) and checks again on the staged file. The helper therefore needs no flags file, and `js-mac-session` is unchanged:
  it takes no `--flags-file`.
- The package's version directory is hostkeeper's, so the package has no versioned directories and no `current` link of
  its own.
- The package ships no pf job. An agent started with `--native-network on` loads the pool's pf anchor itself, with the
  policy of its own pf flags, the one its checks read (`crates/js-mac-provision/src/keep.rs`): at its start, before the
  check it makes then, it checks the anchor and installs it unless it is in force; a first apply that is refused (for
  example before the system's own pf start at boot) is tried once more on the next tick, 30 seconds later; and every
  30 seconds after that, on a thread of its own, it does the same until it exits (a failed tick is logged and the next
  one tries again). The admission check of a job only checks. An edit of the flags file reaches the anchor at the
  agent's next start. The rules stay in the kernel while the agent is down.
- The manifest: `package` `com.komira.js-agent`, `run: ["js-agent", "js-agent.session"]` (the
  agent first, so the observer reports the agent's version), the hook command `["/Library/Application Support/js-agent/bin/js-agent",
  "update-hook"]`, `hook.drain` `drain` and `hook.resume` `resume`, the same `minHostkeeper` and `installerBounds` as the
  Linux entry, and no `healthUrl` (the Linux entry's names the agent's `--health-listen` route; the Mac agent is started
  without it). The hook's `params` are
  `unit` `js-agent`, `helper_unit` `js-agent.session`, `state_dir`
  `/Library/Application Support/js-agent/update`,
  `release_link` `/var/lib/hostkeeper/packages/js-agent/current`, `flags_file` `/etc/js-agent/flags`, `token_file`
  `/etc/js-agent/token`, `state_root` `/var/db/js-agent/state`, `socket_group` `_jsagent`, `scratch_root`
  `/var/db/js-agent/scratch/runs`, `runtime` `native`, and the cordon, drain and step bounds of the Linux manifest.
  `socket_group` and `scratch_root` are the helper plist's `--socket-group` and the agent plist's `--scratch-root`
  (a test of xtask holds them equal); they name the host setup the hook makes (below).
- `helper_unit` is a launchd-only parameter of the hook (`crates/js-agent`, the update hook). When it is present the
  launchd cutover, and the rollback, boot out and bootstrap the helper's plist from the release tree as well as the
  agent's, the helper before the agent, while the agent is stopped. When it is absent the hook does what it did before.
  `prepare` drains the agent before hostkeeper swaps the files, as on Linux.
- The hook retires the pf job an earlier release ran. On a launchd host, at every `cutover` (the first install
  included), after the helper and before the agent starts, it boots `js-agent.pf-pool` out if launchd has it loaded and
  removes `/Library/LaunchDaemons/js-agent.pf-pool.plist` if it is there; both are no-ops once done. The label is a
  constant of the hook, not a param; an older manifest's `pf_unit` is ignored. The `rollback`, after it has put the
  helper back, restores the job when the previous release's tree carries its plist: it boots the job out, runs the
  previous tree's `js-mac-provision install --flags-file <flags_file>` to its end, puts the plist back at
  `/Library/LaunchDaemons` when that file is absent, and bootstraps it from the previous tree, so that the earlier
  release's agent finds the anchor as it expects. A previous tree without that plist gets none of this.

## Release and desired version

1. `release.yml`'s `beta-macos-pkg` calls `js-agent-macos-pkg.yml` with `version` the release's, `sign: true` and
   `secrets: inherit` (a called workflow reads every secret it is not passed as empty, so without it `sign` would skip).
   A pull request runs none of it. The release's `beta-macos-first-install` (after `beta-macos-pkg`, no secret)
   calls `js-agent-macos-first-install.yml`, which installs the unsigned package on a runner through hostkeeper's
   installer and runs the update hook as root, then rolls a config package over the installed agent and updates the
   agent to a second package (the next patch version, built beside the first and never released); only
   `promote-macos` waits for it, never the deploy. A manual run of
   `js-agent-macos-pkg.yml` calls it too, so a branch's package can be tried before it merges.
2. The `sign` job runs on main for that call (and for a manual run), in the environment `pkg-signing`, with the
   secrets `MACOS_INSTALLER_P12` (the "Developer ID Installer" certificate and key as a base64 PKCS#12 file) and
   `MACOS_INSTALLER_P12_PASSWORD`. It imports them into a temporary keychain for `productsign` alone, finds the
   identity in that keychain by its SHA-1 (`security find-identity -v -p basic`; no name or team id is written in the
   workflow), signs with `productsign --timestamp`, checks the result with `pkg check-macos`, and sums it. Nothing is
   notarized. A Mac admits the package only when its signers file lists the leaf: the identity hostkeeper's own package
   is signed with is the one to check for first; another identity needs each Mac's signers file extended by hand and its
   daemon restarted before the first order, since nothing pushes that file to an enrolled Mac. Without the secrets the
   job says so in a notice and the release has no signed package.
3. `pkg-upload` runs on ubuntu in the environment `apt-publish`, after `publish` passed and `beta-macos-pkg` ended,
   pass or fail (it waits for no test of the beta stage), never on a pull request. It downloads the artifact `js-agent-macos-pkg-signed`, gates on the
   settings as the apt jobs do (`channels gate`), assumes `vars.APT_PUBLISH_ROLE_ARN`, and runs `channels pkg upload
   --dir <dir> --version <V> --bucket <b> --work <dir>`. The verb checks that `<dir>/SHA256SUMS` lists exactly the
   package with its digest, puts the package and then `SHA256SUMS` under the key prefix `pkg/jobscheduler/<V>/` (the one
   prefix the jobscheduler role may write; If-None-Match `*`, single part), and counts a 412 answer as success only when
   the stored object's digest equals the local one. The package is public at
   `https://<APT_HOSTNAME>/pkg/jobscheduler/<V>/js-agent-<V>-macos-arm64.pkg`. A last step of the job runs `channels
   descriptor write --bucket <b> --distribution-id <id> --work <dir>`: it reads `hostkeeper-repository.json` at the
   bucket's root with its ETag and merges in the one part this repository owns, `formats.macos-pkg.path` `pkg`,
   `macos-arm64` among the platforms, and `packages.js-agent` as `{"dir": "jobscheduler"}`, every other key left as
   it was. It writes the result with `If-Match` on the ETag (a `412` reads and decides again, three attempts), writes
   nothing when the descriptor holds the mapping already, and invalidates the path in the CDN. It refuses, writing
   nothing, a descriptor that is absent (hostkeeper's release creates it), of a version newer than 1, not of its form,
   with another `path` for the format, or with `js-agent` in another directory.
4. `release desired --pkg-url <base>` (the base is `https://packages.komira.ai/pkg/jobscheduler`) downloads
   `<base>/<V>/SHA256SUMS` and the package and checks the package's sha256 against it. When they are there it adds the
   `macos-arm64` entry, `{"installer":"macos-pkg","repository":"komira-stable","packageVersion":V,"digest":<sha256 of
   the signed package>,"bytes":"<size>","manifest":<the Mac manifest>}`, and writes `set-mac=true`. When the SHA256SUMS
   is absent (404, or the 403 CloudFront over a bucket it may not list answers for a missing object) it writes
   `set-mac=false` and the targets have no `macos-arm64`, so the Linux entries are unaffected; any other failure is
   an error.
5. `hostkeeper-desired.yml`'s `check` job outputs `set-mac`. Its job `set-desired-mac` calls the same reusable workflow
   at the same pin with `host_group: mac-build` and the same version and targets (a version set again with other
   targets is refused `release_exists`), after `set-desired`, and runs only when both `set` and `set-mac` are `true`.
   If the coordinator's entry for this repository names `host_groups`, it must list `mac-build`. hostkeeper's desired
   version is per host group and package, so the Macs can stay on a version while the Linux group moves, and the other
   way round.

## Host setup and the group, once per Mac

Nobody has a root shell on the Macs: everything a Mac needs arrives through hostkeeper, as a package, its files or
its hook. The flags file and the CA come from the group's config package, above, and the flags file names hostkeeper's
host certificate and key (`/var/lib/hostkeeper/identity/host.crt` and `host.key`), so a Mac needs no
`/etc/js-agent/token` (and no instance file: that is for a token minted for a cloud instance). The rest is made by the
update hook's `prepare`, which hostkeeper runs as root, before it writes its record or cordons the host, on a first
install and on every update (`crates/js-agent/src/update_hook/step/host_setup.rs`):

- the helper's socket group (`socket_group`, `_jsagent`): made with `dseditgroup -o create` when the local directory
  does not list it, under the gid macOS picks; a group with staff's gid (20, the lease users' group the helper's
  plist leaves at its default) is refused, as the helper refuses it;
- the agent's state root (`state_root`) and scratch root (`scratch_root`, `/var/db/js-agent/scratch/runs`), mode 0700,
  root's, with `/var/db/js-agent` 0755;
- a file system of its own for the scratch root (the agent refuses a state root on the scratch root's device): while
  the scratch root's parent, `/var/db/js-agent/scratch`, shares a device with the state root, the hook gives that
  directory the APFS volume `js-agent-scratch` in the Data volume's container, with a quota of nine tenths of the
  container so that jobs that fill it leave room for the system and the state root. A volume of that name that is
  there already is used. The volume is made unmounted (`diskutil apfs addVolume <container> APFS js-agent-scratch
  -quota <N>G -nomount`) and mounted on the scratch root's parent (`diskutil mount nobrowse -mountPoint
  /var/db/js-agent/scratch <device>`, unmounted first if it is mounted elsewhere; a Mac whose volume is mounted there
  already needs no mount); a volume root that ignores ownership is given it (`diskutil enableOwnership`, which macOS
  keeps for the volume across mounts and boots). A volume rather than a disk image or a second partition: it needs
  no space set aside and shares the container's free space up to its quota. The volume is mounted on the parent, not
  on the scratch root, because the bundle store, `--bundle-store /var/db/js-agent/scratch/bundles`, must lie on the
  scratch volume and cannot lie in the scratch root: the agent's start removes every entry of the scratch root that
  is not a run's directory. The mount point is root's, mode 0711 (search only for others): a native job runs as a
  lease user, which must walk down through it to a bundle's programs in the store, and cannot list it; the scratch
  root on the volume is root's, 0700. The hook makes no store directory: the agent makes it at its start (below);
- no file system table line. Since macOS 26.6 the system's sandbox lets only a process with Full Disk Access (a TCC
  grant, which root does not imply) write `/etc/fstab`, and answers any other process with `EPERM`; hostkeeper's
  daemon has no such grant and a Mac without device management has no way to give it one. So the agent mounts the
  volume itself, at each start, before it opens either root (`crates/js-agent/src/lifecycle/scratch_volume.rs`,
  `mount_at_start`): while the scratch root's parent is on the state root's file system (as after every boot), it finds
  `js-agent-scratch` in the Data volume's container and mounts it as above, and a start that cannot ends with the
  reason and is started again by launchd. A boot thus needs nothing that macOS refuses, and `verify` sees the mount:
  an agent whose roots share a file system refuses to start, so it says no hello. An earlier release's copy of the
  table beside it (`/etc/fstab.js-agent`) is removed by the setup.

- the bundle store, when the host's flags give `--bundle-store`, is made by the agent at its start, after it has
  mounted the volume: the directory, root's, 0711, when it is missing. The agent refuses to start (exit 2, naming the
  directory) for a store inside the scratch root, and for a store with a directory above it, or itself, that a lease
  user cannot search (not a directory, not root's, writable by group or others, or without `o+x`)
  (`crates/js-agent/src/bundle/layout.rs`); the lab's config package may therefore keep
  `--bundle-store=/var/db/js-agent/scratch/bundles`.

Each part is looked at before it is changed and only what is not as required is made (the owner and mode of the state
root, the mount point and the scratch root are set on every run), so a rerun makes nothing on a Mac that is set up, and a run killed at any point and run
again ends as an uninterrupted one. A part that cannot be read or made defers the step
(exit 75, `host_setup_missing`, the part and the cause in the detail) with no record written and no cordon placed;
hostkeeper orders the host again later. A system call the system refuses (`EPERM`), which no retry changes, fails the
step instead (exit 1, `host_setup_refused`, class environment, the path and the error in the detail), with no record
written and no cordon placed, so that the order fails where an operator sees it. No step undoes the setup: it belongs
to the host, not to a release.

Not made by the hook:

- the pool's users, which `js-mac-session` makes in the uid range of its plist (600-699) when it starts;
- for `--native-network on`, the pool's pf anchor, which the agent loads itself at its start and every 30 seconds
  after it.

What a restart or a macOS update of a Mac needs, and how to check a Mac after one, is
[docs/runbooks/mac-restart-and-update.md](../runbooks/mac-restart-and-update.md).

Once, for the repository of packages: the signers file of each Mac, the environment `pkg-signing` with its two secrets, and the publish role's
write access to the prefix `pkg/jobscheduler/`.

The group lists the package with `host-groups put` (the whole group, `hostkeeper` kept); a package added is installed
once the group has a desired version for it, so the group may list js-agent before any release can be ordered onto a
Mac. Until the group lists the package, a `set-desired` for the group is refused `not_in_host_group`.

The lease server's agent pool configuration gains a pool the Macs meet: `capabilities = { os = "macos", arch =
"arm64", layered-rootfs = "true" }` and `exec = "sandbox-exec"`. A joined pool places a host by the capabilities it
names, so a job with mounts (which asks for `layered-rootfs`, as kbf's macOS action jobs do) is placed on a Mac only
when the pool names it; every native agent of a release that takes mounts reports it, and one that does not is refused
at its hello. The pool may also name a least `os_version`; a host is placed by the version its hello reports. The
Macs' pool names `os_version = "26"` (`docs/design/mac-runtime.md`, section 5). The Macs must reach its agent listener.

## The pool's users stay for good

The helper (`js-mac-session`) makes the pool's users (`kbf-lease-pool-<n>`) once and never deletes one, nor a home, nor a
per-user folder the system placed: measured on macOS 27.0.1 with System Integrity Protection on, root (from a launchd
daemon and from an ssh root shell) may not delete a local user record, may not remove an existing account's home, and
may not remove `/private/var/folders/<xx>/<hash>`, though everything can be emptied. A lease's end resets its user in
place, and a first install on a Mac that already has the pool's users (an earlier install left them) adopts the ones
that are the pool's own, so a package can be installed again and again. Uninstalling the package therefore leaves the
users: a Mac that must lose them is reset by its administrator.

The release signs the helper, and a management profile in the repository (a privacy-policy payload for the helper's
installed path and its designated requirement, which needs device management) grants it Full Disk Access. With the grant
the helper can delete a user's record and home, and the one deletion that is left, of a lease user from before the pool,
would work. The helper's `full-disk-access` verb reports whether it holds the grant (it lists the privacy database's
directory, which only a process with the grant may list), and the agent reports the answer as its capability
`full-disk-access`. The pool's reset described here is unchanged by the grant; deleting an account per job is the next
step (`mac-account-per-job.md`).

## Platform properties

The hello reports `os=macos`, `arch=arm64` and `os_version`, the major version of the Mac's `ProductVersion`. A
job's `constraints.os_version` (`26`: macOS 26 or newer) is compared with it as at least, by major version.

## Order

1. This document.
2. The package relayout, the new labels, the cdhash in the helper's plist, `helper_unit` in the update hook, the
   workflow and its lints, `packaging/js-agent-macos/README.md`, `docs/release.md` and the goldens.
3. The signing environment and the upload role (operator), then the release jobs and the `macos-arm64` entry.
4. `host-groups put` and the group's config package (operator), then the first `set-desired` for `mac-build`; the
   hook makes the host setup.
5. kbf asks for `os=macos` (kbf).

## Failure modes

- Unsigned, or signed by an identity a Mac does not list: the order fails `signature` on that Mac; nothing is
  installed, and Linux hosts are unaffected.
- A `Bom` that does not match the payload, an entry not root's or with a setuid bit, a `run` label without its plist,
  or a `PackageInfo` that does not match: `stage` refuses it. `pkg check-macos` refuses the same shapes in CI, before
  the release.
- A `js-agent` whose cdhash is not the one in the helper's plist (a binary changed after the build): the helper refuses
  the agent, and `build` refuses the package first, because it compares the cdhash of the staged binary with the one in
  the plist.
- No flags file on the Mac, or one that names no host certificate on a Mac with no token: a first install's `prepare`
  defers `join_token_missing` (exit 75) having installed nothing, and hostkeeper runs the hook's `rollback` and retries
  later. A later order installs it once the host setup is done.
- A part of the host setup the hook cannot make (the socket group, a root, the scratch volume or its mount):
  `prepare` defers `host_setup_missing` (exit 75) naming it, with no record written and no cordon placed, and
  hostkeeper runs the hook's `rollback` (a first install) and retries later. A system call of the setup that macOS
  refuses (`EPERM`) fails `prepare` instead (`host_setup_refused`, exit 1), naming the path and the error, and is not
  retried as if it could pass. `cutover` never starts an agent whose roots share a device or a helper whose socket
  group is missing.
- The agent cannot mount the scratch volume at a start (no `js-agent-scratch` in the Data volume's container, or
  `diskutil` fails): the agent exits naming `--scratch-root` and the cause, and launchd starts it again after its
  throttle interval; during an update `verify` sees no hello and the release is rolled back.
- No signed package for a release (the environment `pkg-signing` is not set up, or `beta-macos-pkg` failed): `pkg-upload`
  uploads nothing, `release desired` writes `set-mac=false`, and the Linux nodes are set as before.
- The entry does not resolve (no `macos-pkg` format in the descriptor, platform missing, no `packages` entry for
  js-agent; the release writes the last two, and the first is the descriptor's own): the Mac shows `no_build_for_host`.
- `set-desired` for `mac-build` before the group lists js-agent: refused `not_in_host_group`; nothing changes.

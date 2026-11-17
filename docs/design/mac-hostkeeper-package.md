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
  `os=macos`, `arch=arm64` on an Apple silicon Mac. It reports no `os_version` capability (bundle admission reads the
  host's version locally, `crates/js-agent/src/bundle/host.rs`).
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
`jobscheduler`, needs one `packages` entry in the descriptor that maps `js-agent` to the directory `jobscheduler`; an
operator makes it once. An entry that does not resolve leaves the Mac `no_build_for_host` and touches no Linux host.

## The package

`js-agent-<version>-macos-arm64.pkg`, identifier `com.komira.js-agent` (`crates/xtask/src/macpkg.rs`, `IDENTIFIER`),
version the release's, a component package built by `pkgbuild` and wrapped by `productbuild` with that one component.
It has no `preinstall` or `postinstall` script and its `Distribution` has no script element. Everything is root's,
group wheel; directories are 0755 unless noted.

| path | mode | |
|---|---|---|
| `/Library/Application Support/js-agent/bin/js-agent` | 0755 | the agent |
| `/Library/Application Support/js-agent/bin/js-mac-session` | 0755 | the root helper |
| `/Library/Application Support/js-agent/bin/js-mac-provision` | 0755 | the plist generator |
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

- The labels are `js-agent` and `js-agent.session` (`crates/js-mac-provision/src/daemon.rs`: `AGENT_LABEL`,
  `SESSION_LABEL`; the plist goldens; the update hook's launchd tests). The two `js-mac-provision` verbs
  of this package are `agent-plist`, which takes no flag and prints the fixed agent plist, and
  `session-plist --daemon-requirement <REQ>`; its pf anchor verbs are unchanged.
  Its `postinstall` verb is gone, with its link and its tests.
- The agent plist is fixed text, checked for drift, never edited on a host. Its program arguments are
  `/Library/Application Support/js-agent/bin/js-agent --flags-file /etc/js-agent/flags --token-file
  /etc/js-agent/token --state-root /var/db/js-agent/state --scratch-root /var/db/js-agent/scratch
  --native-helper-socket /var/run/js-mac-session/socket`; it runs as root, with `RunAtLoad` false, `ThrottleInterval`
  30, `ExitTimeOut` 120 and its logs in `.../js-agent/log/js-agent.log` and `js-agent.err.log`. `KeepAlive` is
  conditional on `/etc/js-agent/token` (`PathState`). It names no `--server` and no `--pool`: a host's own values (the
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
- The manifest: `package` `com.komira.js-agent`, `run: ["js-agent", "js-agent.session"]` (the agent first, so the
  observer reports the agent's version), the hook command `["/Library/Application Support/js-agent/bin/js-agent",
  "update-hook"]`, and the same `minHostkeeper` and `installerBounds` as the Linux entry. The hook's `params` are
  `unit` `js-agent`, `helper_unit` `js-agent.session`, `state_dir` `/Library/Application Support/js-agent/update`,
  `release_link` `/var/lib/hostkeeper/packages/js-agent/current`, `flags_file` `/etc/js-agent/flags`, `token_file`
  `/etc/js-agent/token`, `state_root` `/var/db/js-agent/state`, `runtime` `native`, and the cordon, drain and step
  bounds of the Linux manifest.
- `helper_unit` is a launchd-only parameter of the hook (`crates/js-agent`, the update hook). When it is present the
  launchd cutover, and the rollback, boot out and bootstrap the helper's plist from the release tree as well as the
  agent's, the helper before the agent, while the agent is stopped. When it is absent the hook does what it did before.
  `prepare` drains the agent before hostkeeper swaps the files, as on Linux.

## Release and desired version

1. `release.yml`'s `beta-macos-pkg` calls `js-agent-macos-pkg.yml` with `version` the release's and `sign: true`. A
   pull request runs none of it.
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
   `https://<APT_HOSTNAME>/pkg/jobscheduler/<V>/js-agent-<V>-macos-arm64.pkg`.
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

Done by an operator as root, never by a package, and before the group's first `set-desired`:

- `/etc/js-agent/token` (root's, 0600: a secret, which no package carries) and `/etc/js-agent/instance` (it differs by
  host); the flags file and the CA come from the group's config package, above;
- the agent's state and scratch roots on different devices (the agent refuses a state root on the scratch root's
  device);
- the helper's socket group `_jsagent` (the plist names it), and the pool, which `js-mac-session` makes users in
  (`pool-create`);
- for `--native-network on`, the pool's pf anchor (`js-mac-provision install`).

Once, for the repository of packages: the descriptor's `packages` entry that maps `js-agent` to the directory
`jobscheduler`, the signers file of each Mac, the environment `pkg-signing` with its two secrets, and the publish role's
write access to the prefix `pkg/jobscheduler/`.

The group lists the package with `host-groups put` (the whole group, `hostkeeper` kept); a package added is installed
once the group has a desired version for it, so the group may list js-agent before any release can be ordered onto a
Mac. Until the group lists the package, a `set-desired` for the group is refused `not_in_host_group`.

The lease server's agent pool configuration gains a pool the Macs meet: `capabilities = { os = "macos", arch =
"arm64" }` and `exec = "sandbox-exec"`. The Macs must reach its agent listener.

## Platform properties

The hello reports `os=macos` and `arch=arm64`. An `os_version` capability, the Mac's `ProductVersion` compared as
at-least, is a separate change in `js-caps` and the agent; until it lands a job must not ask for one.

## Order

1. This document.
2. The package relayout, the new labels, the cdhash in the helper's plist, `helper_unit` in the update hook, the
   workflow and its lints, `packaging/js-agent-macos/README.md`, `docs/release.md` and the goldens.
3. The signing environment and the upload role (operator), then the release jobs and the `macos-arm64` entry.
4. Host setup, `host-groups put` and the group's config package (operator), then the first `set-desired` for
   `mac-build`.
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
- No token on the Mac: the agent does not start, so the hook's `verify` (the agent runs and is registered) fails;
  hostkeeper runs the hook's `rollback` and, a first install having no previous version, removes the package. A later
  order installs it again once the host setup is done.
- No signed package for a release (the environment `pkg-signing` is not set up, or `beta-macos-pkg` failed): `pkg-upload`
  uploads nothing, `release desired` writes `set-mac=false`, and the Linux nodes are set as before.
- The entry does not resolve (no `macos-pkg` format in the descriptor, platform missing, no `packages` entry for
  js-agent): the Mac shows `no_build_for_host`.
- `set-desired` for `mac-build` before the group lists js-agent: refused `not_in_host_group`; nothing changes.

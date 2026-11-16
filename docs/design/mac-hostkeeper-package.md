# js-agent on a Mac, installed by hostkeeper

How js-agent reaches a Mac of a hostkeeper host group (such as `mac-build`) the way it reaches a Linux build node: a
release of this repository carries a signed macOS package, hostkeeper's coordinator orders it onto the group's Macs,
and the agent registers with the lease server and offers native jobs in `sandbox-exec` pool mode. It says what changes
here, what hostkeeper requires, and which steps are outside this repository. Nothing here is built yet.

## Where things stand

- `.github/workflows/js-agent-macos-pkg.yml` builds `js-agent-<version>-aarch64-apple-darwin.pkg` unsigned, as a
  workflow artifact kept seven days: in the release's beta stage (`beta-macos-pkg`, as version `0.0.0`) or by hand.
  Its `sign` job runs only by hand from main, in the environment `macos-signing`, signs and notarizes; without that
  environment's secrets it skips with a notice. No release lists the package.
- The package lays down `/usr/local/js-agent/releases/<version>/`, plist templates in `/usr/local/js-agent/share/`,
  `/Library/Logs/js-agent/` and `/var/db/js-agent-update/`. Its `postinstall` (`js-mac-provision postinstall`) makes
  `link/current` and installs the two plists into `/Library/LaunchDaemons` once, under the labels
  `com.komira.js-agent` and `com.komira.js-mac-session`, with placeholders an operator edits
  (`packaging/js-agent-macos/README.md`).
- `cargo xtask release desired` (`crates/xtask/src/release/desired.rs`) hands hostkeeper one `apt` entry per Linux
  platform and no `macos-arm64` entry; its manifest is the Linux one (`runtime = "crun"`, one systemd unit).
  `hostkeeper-desired.yml` sets js-agent for the host group `all-in-one` alone.
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
start applies only to an entry without one). The package's own versioned directories and `current` link duplicate
hostkeeper's and are dropped.

The coordinator resolves a release's `macos-arm64` entry in the named-version form (`installer: macos-pkg`,
`repository`, `packageVersion`, `digest`, `bytes`, `manifest`) through the repository's descriptor, whose
`formats.macos-pkg` gives the path and platforms, to
`<base>/<macos-pkg path>/js-agent/<version>/js-agent-<version>-macos-arm64.pkg`, and streams it to the Mac; the Mac
needs no store of its own. The directory defaults to the service's name, so the descriptor needs no `packages` entry
for js-agent. An entry that does not resolve leaves the Mac `no_build_for_host` and touches no Linux host.

## The package after this change

| path | |
|---|---|
| `/Library/Application Support/js-agent/bin/{js-agent,js-mac-session,js-mac-provision}` | the programs |
| `/Library/Application Support/js-agent/LICENSE` | |
| `/Library/LaunchDaemons/js-agent.plist` | label `js-agent`: the agent |
| `/Library/LaunchDaemons/js-agent.session.plist` | label `js-agent.session`: the root helper |

- Identifier `com.komira.js-agent` (`crates/xtask/src/macpkg.rs`, `IDENTIFIER`, unchanged); version the release's;
  file name `js-agent-<version>-macos-arm64.pkg`. Plists 0644, directories 0755, all root's.
- The labels change from `com.komira.js-agent` and `com.komira.js-mac-session` to `js-agent` and `js-agent.session`
  (`crates/js-mac-provision/src/daemon.rs`, the plist goldens, the update hook's launchd tests).
- The plists are fixed text, checked for drift, never edited on a host. They carry what is the same on every Mac:
  `--flags-file`, `--token-file /etc/js-agent/token`, the state and scratch roots, the helper's socket. A host's own
  values (the lease server and its CA, the pool, the helper's caller requirement and socket group,
  `--native-network`) go to `/etc/js-agent/flags` and `/etc/js-agent/session.flags`, which are not in the payload and
  which hostkeeper never touches. js-agent reads `--flags-file` already (`crates/js-agent/src/flags_file.rs`: a
  regular file, root's or the agent's, not writable by group or others, one word per line); `js-mac-session` gains the
  same flag through the same code.
- The log directory and the hook's state directory leave the payload: they move to
  `/Library/Application Support/js-agent/{log,update}`, which the payload does not list, so neither an update nor a
  removal touches them. The hook's `prepare` makes them before the first `cutover` starts a daemon (launchd does not
  make a log file's directory).
- `KeepAlive` stays conditional on `/etc/js-agent/token`.
- No `postinstall`: `js-mac-provision postinstall` and its link go.
- The manifest: `package` `com.komira.js-agent`, `run: ["js-agent", "js-agent.session"]` (the agent first, so the
  observer reports the agent's version), the hook `/Library/Application Support/js-agent/bin/js-agent update-hook`
  with `runtime = "native"` and the launchd form of the unit port (`unit` `js-agent`), the same step bounds,
  `minHostkeeper` and installer bounds as the Linux entry. `prepare` drains the agent before hostkeeper swaps the
  files, as on Linux.

## Release and desired version

1. `release.yml`'s `beta-macos-pkg` passes the release's version instead of `0.0.0`.
2. The `sign` job becomes a job of the release run on a push to main, after the three beta jobs passed (the gate
   `promote-macos` already uses), never on a pull request. It signs with `productsign --timestamp`, checks the result
   with `pkg check-macos` and sums it. Notarization is dropped: hostkeeper does not need it, and a package no person
   double-clicks gains nothing from it. A Mac admits the package only when its signers file lists the leaf: the
   identity hostkeeper's own package is signed with is the one to check for first; another identity needs each Mac's
   signers file extended by hand and its daemon restarted before the first order, since nothing pushes that file to an
   enrolled Mac.
3. An upload job writes the signed package to `<macos-pkg path>/js-agent/<version>/` of the package repository, with
   a short-lived role allowed that prefix.
4. `release desired` adds the `macos-arm64` named-version entry with the signed package's digest and size.
   `hostkeeper-desired.yml` calls `set-desired` a second time with `host_group: mac-build` and the same targets (a
   version set again with other targets is refused `release_exists`). If the coordinator's entry for this repository
   names `host_groups`, it must list `mac-build`. hostkeeper's desired version is per host group and package, so the
   Macs can stay on a version while the Linux group moves, and the other way round.

## Host setup and the group, once per Mac

Done by an operator as root, never by the package, and before the group's first `set-desired`:

- `/etc/js-agent/{token,flags,session.flags,ca.pem}`, root's, the token mode 0600;
- the agent's state and scratch roots on different devices (the agent refuses a state root on the scratch root's
  device);
- the helper's socket group (the operator makes it; the helper is told its name), and the pool, which `js-mac-session`
  makes users in (`pool-create`);
- for `--native-network on`, the pool's pf anchor (`js-mac-provision install`).

Then `host-groups put` of `mac-build` with `[hostkeeper, js-agent]` (the whole group, `hostkeeper` kept). Until the
group lists the package, a `set-desired` for the group is refused `not_in_host_group`.

The lease server's agent pool configuration gains a pool the Macs meet: `capabilities = { os = "macos", arch =
"arm64" }` and `exec = "sandbox-exec"`. The Macs must reach its agent listener.

## Platform properties

The hello reports `os=macos` and `arch=arm64`. An `os_version` capability, the Mac's `ProductVersion` compared as
at-least, is a separate change in `js-caps` and the agent; until it lands a job must not ask for one.

## Order

1. This document.
2. The package relayout, the new labels and `js-mac-session --flags-file`: `macpkg.rs`, `js-mac-provision`,
   `js-mac-session`, the update hook's launchd form, the workflow and its lint, `packaging/js-agent-macos/README.md`,
   `docs/release.md` and the goldens.
3. The signing environment and the upload role (operator), then the release jobs and the `macos-arm64` entry.
4. Host setup and `host-groups put` (operator), then the first `set-desired` for `mac-build`.
5. kbf asks for `os=macos` (kbf).

## Failure modes

- Unsigned, or signed by an identity a Mac does not list: the order fails `signature` on that Mac; nothing is
  installed, and Linux hosts are unaffected.
- A `Bom` that does not match the payload, an entry not root's or with a setuid bit, a `run` label without its plist,
  or a `PackageInfo` that does not match: `stage` refuses it. `pkg check-macos` refuses the same shapes in CI, before
  the release.
- No token on the Mac: the agent does not start, so the hook's `verify` (the agent runs and is registered) fails;
  hostkeeper runs the hook's `rollback` and, a first install having no previous version, removes the package. A later
  order installs it again once the host setup is done.
- The entry does not resolve (no `macos-pkg` format in the descriptor, platform missing): the Mac shows
  `no_build_for_host`.
- `set-desired` for `mac-build` before the group lists js-agent: refused `not_in_host_group`; nothing changes.

# js-agent on a Mac

A signed installer package (`js-agent-<version>-aarch64-apple-darwin.pkg`) that lays down the
agent, its root helper and the install-time tool, installs two LaunchDaemons and loads them.
It is built by `.github/workflows/js-agent-macos-pkg.yml` and read back by
`cargo xtask pkg check-macos`; `cargo xtask pkg stage-macos` is the one place that lists
what the package holds (`crates/xtask/src/macpkg.rs`).

## What is installed

| path | |
|---|---|
| `/usr/local/js-agent/releases/<version>/{js-agent,js-mac-session,js-mac-provision,LICENSE}` | one release directory; the update hook stages the next one beside it |
| `/usr/local/js-agent/link/` | empty; `postinstall` makes `current` in it, once, and the update hook moves it after |
| `/usr/local/js-agent/share/` | the two plists and the updater declaration, as templates |
| `/Library/Logs/js-agent/` | the daemons' logs |
| `/var/db/js-agent-update/` | the update hook's state directory, root's, mode 0700 |

Both daemons run their program through `/usr/local/js-agent/link/current`.

## The daemons

`com.komira.js-mac-session` (the root helper) and `com.komira.js-agent` are described by flags
only (`js-mac-provision agent-plist`, `js-mac-provision session-plist`; the goldens are in
`crates/js-mac-provision/tests/golden`). The package ships them with placeholders for the
values of a host (the lease server, the helper's caller requirement and socket group), which
the programs refuse at start.

Neither runs at load. `KeepAlive` holds each while `/etc/js-agent/token` exists, so the
package loads them at install time and they start when the host is given its join token.
`ThrottleInterval` is 30 seconds: a refusal is retried every 30 seconds and is read from
`/Library/Logs/js-agent/js-agent.err.log`.

## What `postinstall` does

The script only runs `js-mac-provision postinstall --version <v> --volume "$3"` from the release
it installed (`crates/js-mac-provision/src/postinstall.rs`). That makes the release link if
there is none, installs a plist if it is not installed (an upgrade never overwrites the one
the operator edited), and runs `launchctl bootstrap` for a daemon that is not loaded. It
never stops, restarts or kills anything: a new release takes effect when the update hook
restarts the agent. Nothing is loaded when the volume is not `/`.

## Signing and notarization

The workflow has two jobs. `build` runs on pull requests and by hand, builds the package
unsigned, checks it and writes `SHA256SUMS`, and names no secret: no code of a branch can see a
signing key. `sign` runs only by hand from main, in the environment `macos-signing`. It does
not build: it takes the checked package and the checker from `build`, checks the package again,
signs it with `productsign`, notarizes and staples it, checks it again and sums it. When the
environment or its secrets are not set up it prints a notice and skips every step after the
first, and the run succeeds.

`macos-signing` is created by a repository administrator, with a rule that only main may
deploy to it and with these secrets as environment secrets (none is in the tree):

- `MACOS_INSTALLER_CERT_P12`: the "Developer ID Installer" certificate and key, a .p12 file, base64.
- `MACOS_INSTALLER_CERT_PASSWORD`: its password.
- `MACOS_INSTALLER_IDENTITY`: the certificate's name, as `productsign --sign` takes it.
- `MACOS_NOTARY_KEY_P8`: an App Store Connect API key, the text of the .p8 file.
- `MACOS_NOTARY_KEY_ID`: its key id.
- `MACOS_NOTARY_ISSUER_ID`: its issuer id.

Each is named only in the environment of the step that needs it. The key is imported for
`productsign` alone. The certificate password reaches `security import` as an argument, on a
runner that exists for this job alone. The version typed for a manual run is checked as
MAJOR.MINOR.PATCH before it is used in a file name.

## Not done yet

- The package is a workflow artifact for seven days, not a release asset: `release.yml` lists
  its assets by name and attests `*.tar.gz`.
- The updater declaration is shipped under `share/`; putting it in `/etc/updater.d` belongs to enrolment.
- The update hook (`js-agent update-hook`) runs its five steps on a Mac, with a `launchctl` form of its unit port (`bootout`, `bootstrap`, `kickstart -k`,
  `print`) and `runtime = "native"` in `updater.toml`, over the lease server's own-host routes
  and the agent's state root, which `updater.toml` names beside the release link (`server`,
  `state_root`, `token_file`). The
  port has run against a fake of `launchctl` only, never against a real launchd: what
  `launchctl print` prints of a daemon that crashed (its `last exit code`), the time a
  `bootstrap` takes to start the daemon, and `bootout` waiting out the agent's `ExitTimeOut`
  are read from the manual and from what the fake says, not from a Mac. `kickstart -k` runs a
  daemon whatever its `KeepAlive` condition says, so the port refuses it while
  `/etc/js-agent/token` is absent.
- The agent's state and scratch directories (`/var/db/js-agent/{state,scratch}`, on separate
  volumes, as the agent requires) and the helper's socket group are host setup.

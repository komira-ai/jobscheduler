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

The workflow always builds the unsigned package and its `SHA256SUMS`. It signs when the
repository holds the secrets below, and notarizes when it also holds the notary key; without
them those steps are skipped and the job stays green. The names, all of them secrets of the
repository (none is in the tree):

- `MACOS_INSTALLER_CERT_P12`: the "Developer ID Installer" certificate and key, a .p12 file, base64.
- `MACOS_INSTALLER_CERT_PASSWORD`: its password.
- `MACOS_INSTALLER_IDENTITY`: the certificate's name, as `productsign --sign` takes it.
- `MACOS_NOTARY_KEY_P8`: an App Store Connect API key, the text of the .p8 file.
- `MACOS_NOTARY_KEY_ID`: its key id.
- `MACOS_NOTARY_ISSUER_ID`: its issuer id.

The certificate password reaches `security import` as an argument, on a runner that exists
for this job alone.

## Not done yet

- The package is a workflow artifact for seven days, not a release asset: `release.yml` lists
  its assets by name and attests `*.tar.gz`.
- The updater declaration is shipped under `share/`; putting it in `/etc/updater.d` belongs to enrolment.
- The update hook (`js-agent update-hook`) is built on Linux only, so a Mac's js-agent does not
  have the verb at all and the hook cannot run there. Building it on a Mac needs a `launchctl`
  form of the hook's unit port (`bootout`/`kill`, `kickstart`, `print`) and the cutover of the
  open update-hook change; until then `updater.toml` is the declaration the hook will take.
- The agent's state and scratch directories (`/var/db/js-agent/{state,scratch}`, on separate
  volumes, as the agent requires) and the helper's socket group are host setup.

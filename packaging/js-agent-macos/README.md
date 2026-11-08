# js-agent on a Mac

A signed installer package (`js-agent-<version>-aarch64-apple-darwin.pkg`) that lays down the
agent, its root helper and the install-time tool, installs two LaunchDaemons and loads them.
It is built by `.github/workflows/js-agent-macos-pkg.yml` and read back by
`cargo xtask pkg check-macos`; `cargo xtask pkg stage-macos` is the one place that lists
what the package holds (`crates/xtask/src/macpkg.rs`).

## What is installed

| path | |
|---|---|
| `/usr/local/js-agent/releases/<version>/{js-agent,js-mac-session,js-mac-provision,LICENSE}` | one release directory |
| `/usr/local/js-agent/link/` | empty; `postinstall` makes `current` in it, once |
| `/usr/local/js-agent/share/` | the two plists, as templates |
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

The workflow has two jobs. `build` (the check `js-agent-macos-pkg`) runs on every pull request
and by hand; it builds the package unsigned, checks it and writes `SHA256SUMS`, and names no secret: no
code of a branch can see a signing key. `sign` runs only by hand from main, in the environment `macos-signing`. It does
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

- The package is a workflow artifact for seven days, not a release asset: `artifacts.yml` (which
  `release.yml` calls) lists the release's assets by name and attests `*.tar.gz`, `*.tar.zst` and
  the index. It no longer carries an
  update declaration, and nothing in it moves `link/current`: the agent is updated as a
  hostkeeper package (`hostkeeper/`, a release asset), which runs its own launchd job.
- The update hook (`js-agent update-hook`) runs its five steps on a Mac as hostkeeper's hook, with a `launchctl` form of its unit port (`bootout`, `bootstrap`, `kickstart -k`,
  `print`) and `runtime = "native"` in the `[params]` of `hostkeeper/manifest.toml.in`, over the lease server's own-host routes
  and the agent's state root, which the params name beside the release link (`state_root`,
  `token_file`; the server comes from the flags file). The
  port has run against a fake of `launchctl` only, never against a real launchd: what
  `launchctl print` prints of a daemon that crashed (its `last exit code`), the time a
  `bootstrap` takes to start the daemon, and `bootout` waiting out the agent's `ExitTimeOut`
  are read from the manual and from what the fake says, not from a Mac. `kickstart -k` runs a
  daemon whatever its `KeepAlive` condition says, so the port refuses it while
  `/etc/js-agent/token` is absent.
- The agent's state and scratch directories (`/var/db/js-agent/{state,scratch}`, on separate
  volumes, as the agent requires) and the helper's socket group are host setup.

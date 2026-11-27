# macOS builds on the build farm's Macs

The release builds two things only a Mac can build: the Apple silicon archives (`js`, `js-agent`,
`js-bundle` and `js-mac-session` for `aarch64-apple-darwin`) and the js-agent
installer package. By default a GitHub-hosted Apple silicon runner builds them. When the repository
variable `MACOS_ON_FARM` is `true`, a Mac of the build farm builds them instead, from a hosted Linux
runner, and the hosted macOS jobs for them do not run. This page says how, what moved and what did
not and why, and what a farm Mac cannot do. (The build farm here is the remote-execution service of
[farm.md](farm.md), not the hosts js-agent runs on.)

The farm builds the same files by the same commands as the hosted runner does, so the archives and
the package are the same in every way a user or a host can see: the names, the checks and the
version they print. Nothing about a release changes but where the Mac compiles.

## Mechanism: one remote-execution action from a Linux runner

A job of `.github/workflows/macos-farm.yml` runs on a hosted Linux runner. The Mac work is one
Remote Execution API action whose platform names the farm's Mac workers, sent by
`cargo xtask mac-farm` (`crates/xtask/src/mac_farm.rs`), a client of that API that runs one Cargo
command of this workspace on a Mac:

```
cargo xtask mac-farm --address <grpc://host:port> --instance <name> --tls <true|false> \
  --properties <key=value,...> --scratch <dir> \
  [--env NAME=VALUE]... [--output <path>]... [--download-to <dir>] \
  -- <cargo arguments>
```

A Mac of the farm runs an action as an ordinary user under a sandbox with no network, and has the
Command Line Tools and no Rust. So everything the command needs travels with the action as its
inputs:

1. the files git tracks in the checkout, as they are on disk (the job stamped the release version
   into them first, so the binaries print it);
2. the crates of `Cargo.lock` for `aarch64-apple-darwin`, which `cargo fetch` downloads on the
   runner into a Cargo home of the run's own;
3. the pinned Rust toolchain for `aarch64-apple-darwin` (`crates/xtask/src/mac_farm/toolchain.toml`:
   the rustc, cargo and rust-std archives by URL and SHA-256).

The checkout's `target/` is not sent. The verb compiles nothing on the runner: `git ls-files` and
`cargo fetch` are all it runs there.

The command is that toolchain's `cargo` with the arguments after `--`, from the checkout's root,
offline, with `PATH` set to the toolchain's `bin` and then the Mac's `/usr/bin:/bin:/usr/sbin:/sbin`.
So a `cargo xtask` verb finds `cargo`, and the build finds the Mac's C compiler and linker, and
`otool`, `codesign`, `pkgbuild`, `productbuild`, `plutil`, `pkgutil` and `lsbom` run from the Mac.
Each `--env` is added to the command's environment (`MACOSX_DEPLOYMENT_TARGET=11.0`, as in the
hosted build).

The blobs the farm's content store lacks are uploaded (it is shared, so a toolchain or a crate it has
is sent once, not per run), the action is executed and followed to its end, and the command's output
is printed.

## Inputs and outputs

Inputs are the three things above. Outputs are the paths given with `--output`, relative to the
checkout's root, and they are new here: the action declares them, and when the command exits 0 the
verb fetches each from the content store, holds it to the digest the farm reported, and writes it at
the same path below `--download-to` (the checkout's root by default). A declared output the command
did not make fails the verb. Nothing is fetched when the command fails.

The release build fetches the four binaries in `target/aarch64-apple-darwin/release/`, and the
package build the package, its `SHA256SUMS`, the program with its sums and the `xtask` the
Mac built (which the signing job checks the signed package with). The Linux runner then does what
needs no Mac: `sha256sum --check` of what it fetched, the archives (`cargo xtask release pack`, which stages each archive as `release stage` does and
writes the `tar.gz` with the options the hosted build uses, so the archives are byte for byte the
same files) and the uploads of the artifacts.

## Authentication

The farm's port is reachable only on its private network. A job joins it with
`tailscale/github-action` and the run's OIDC token (workload identity federation, tag `tag:ci`), so no
secret is stored. The tailnet identity is the organisation variables `TS_CLIENT_ID` and
`TS_AUDIENCE`. The farm's address, instance name, TLS setting and Mac platform properties are the
organisation variables `FARM_ADDRESS`, `FARM_INSTANCE`, `FARM_TLS` and `FARM_DARWIN_ARM64_PROPERTIES`;
no file of the repository holds an endpoint or a property set. Membership of the network is the gate.

A run without the variables (a fork's) fails by name: `xtask mac-farm` exits 1 with `the farm is not
configured: --address is empty (FARM_ADDRESS is not set)` when `--address` is empty, so the verb is
the check and no step of the workflow guards it. No job ends green having built nothing, and no step
is guarded by the address; only the join is guarded, by `TS_CLIENT_ID`. Every `run:` of the workflow
is one command line (the version is checked by `release stamp` and `release next-version`, which
refuse what is not MAJOR.MINOR.PATCH).

The variables are not secrets, so a step's `env:` header in the log shows them; the verb prints
`::add-mask::` for the address before it prints anything else.

### Why the jobs are in a file of their own

The token is needed for the join, and a token is what a signer is made of: the signer identity of an
attestation is the workflow file that ran the signing step, and every host trusts
`.github/workflows/artifacts.yml` on `refs/heads/main` alone (docs/release.md). Any job of
artifacts.yml that held `id-token: write` could mint a token with that identity. So the farm's jobs
are jobs of `macos-farm.yml`, whose tokens name that file, and artifacts.yml and
js-agent-macos-pkg.yml only call it (a call grants the token its called jobs hold, and no more).

The rules that keep this safe are linted (`cargo test -p xtask --test apt_workflow`):

- a job that holds `id-token: write` in a workflow a pull request or a merge group starts, or in one
  such a workflow calls, at any depth, has an `if:` whose top-level `&&` conjuncts pin a push to main
  or a manual run (`github.event_name == 'push'` together with `github.ref == 'refs/heads/main'`
  in the same `&&` conjunction, `'workflow_dispatch'`, or a parenthesised `||` of them), or sits
  below a call that does;
- in artifacts.yml no job but `attest` holds the token, except a call of another workflow file;
- in macos-farm.yml every job that holds the token pins a push or a manual run itself, and runs no
  repository code at that point: no `cargo` command in any `run:` (the verb runs `cargo fetch`
  itself), and no local action. The
  job `xtask`, which holds no token, builds xtask and uploads it, and the jobs that hold the token
  download that binary and run it (`mac-farm`, `release stamp`, `release stage`, `release
  next-version`).

So no pull request, merge group or branch push holds a token that reaches the farm, and no build
script ever runs beside it.

### What the farm Macs are trusted with

With the farm, the unsigned programs and the `js-agent-macos-checker` xtask that the `pkg-signing`
jobs sign and run come from a farm Mac rather than a GitHub-hosted runner. The farm Macs are
therefore trusted with what goes into the signed packages, as GitHub's runners were.

`pkg build-macos` accepts a `dist/` that already exists because a remote action's runner makes the
parent directory of each declared output before the command starts.

## Logs and exit codes

The farm answers with the command's exit code and its stdout and stderr, in full. The verb prints
both and ends 0 only for exit 0. Nothing is streamed while the action runs: the log shows the stage
changes (queued, then running) and then the whole output. When the farm could not finish the job (a
lost Mac, a kill, the time limit) the answer is a status with no result, the farm attaches the end of
the job's output when it kept one, and the verb prints both and ends 1. The farm may answer from its
cache for an action it has run before, and the verb says so ("a cached result").

## Time limit

The action's timeout is `--timeout-secs` (2400 s by default), which the farm holds under its own
maximum. At the deadline the farm kills the command and the action fails. The verb has a connect timeout and no other deadline of its own: a step's `timeout-minutes` bounds a run
that waits in the queue, or a connection that hangs. A Mac that is busy leaves the action queued, and
the log says so; no Mac able to run it at all is refused at once, and the job fails naming the refusal.

## What a Mac action cannot do

The sandbox denies every network operation, loopback and Unix sockets included, and a process under
it cannot apply a sandbox profile of its own. There is no root, no GUI session, no device and no
keychain. So a job that binds a socket, runs something under `sandbox-exec`, installs a package,
loads a launchd job or signs with a key does not run there. The build of the programs and the
package needs none of those: `pkgbuild` and `productbuild` build an unsigned package as a user.

## What moved, and what did not

Moved (with `MACOS_ON_FARM` set to `true`):

- artifacts.yml's Apple silicon archives. The hosted `build-macos` job is skipped and the call job
  `macos-farm` runs `macos-farm.yml` with `what: release`. Both upload `dist-aarch64-apple-darwin`,
  so `sums` downloads either with the Linux legs'; it takes whichever ran.
- js-agent-macos-pkg.yml's `build` and `build-update`, the unsigned package of the release's version
  and of the next patch version. The hosted jobs are skipped and the call job `farm` runs
  `macos-farm.yml` with `what: pkg`, whose jobs `pkg` and `pkg-update` upload the artifacts the hosted
  jobs upload, under the same names. The jobs after them take whichever side ran.

The two sides are mutually exclusive by their conditions (each hosted job's `if:` is the negation of
its farm call's), checked by `cargo test -p xtask --test release_workflow`.

Not moved:

- `sign-code`, `package` and `sign` of js-agent-macos-pkg.yml, the jobs that sign. They hold a
  signing key and use the Mac's keychain and `codesign`, which an action under the sandbox cannot
  use. They wait for a signing design of their own.
- `beta.yml`'s tests that run as root, and `js-agent-macos-first-install.yml`. They install a package
  and load launchd jobs, which needs root on a machine of their own: a macOS virtual-machine job,
  which the farm does not offer to a workflow.
- `macos-probe.yml`. It measures the hosted runner's host (what the native runtime relies on there),
  so it stays on that runner.
- The six manual workflows that build a privileged test binary on macOS (`js-agent-native-privileged-tests.yml`,
  `js-mac-pool-privileged-tests.yml`, `js-mac-provision-privileged-tests.yml`,
  `js-treestore-privileged-tests.yml`, `js-agent-native-isolation-test.yml` and
  `js-agent-native-inbound-pf-test.yml`). Their builds stay hosted for now because their test
  binaries' file names carry a hash that Cargo picks, and an output of an action is declared by path
  before the build, so the verb cannot fetch a file whose name it does not know.

## Switching, and trying a branch

`MACOS_ON_FARM` is a repository variable, not a secret. Unset or any value but `true` builds on the
hosted runners as before; `true` builds on the farm. A pull request runs neither (the beta stage and
the release are after the merge).

To try a branch before the variable is set, run `js-agent-macos-pkg.yml` by hand with the input `farm`
(`gh workflow run js-agent-macos-pkg.yml --ref <branch> -f version=X -f farm=true`): it builds the
package and the update package on the farm, and its `first-install` job installs them on a hosted
runner as it does after a hosted build. `macos-farm.yml` can also be run by hand on a branch with a
version and what to build (`release` or `pkg`); it uploads the artifacts to that run, and publishes
nothing.

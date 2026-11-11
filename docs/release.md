# Releasing js-lease-server, js, js-agent, js-bundle, js-mdm and the Mac host tools

Releases are continuous. Nobody tags, and nothing is run by hand: every push to `main` that
changes code that ships is released as the next version, after the jobs of `ci` other than `macos` have passed on it.

## What happens on each merge to main

1. `ci` runs on the push, as for any push, and the `release` workflow (`.github/workflows/release.yml`)
   starts on the same push. It acts only on a push to `main`, and works on the commit pushed: the
   run's own `github.sha`.
2. `ci-passed` waits for the jobs of the `ci` run of that commit, all but the one named `macos` (it polls every 120 s, for up to four hours; ten failed reads of the runs or their jobs in a row, about twenty minutes, fail it too)
   and passes when every other job succeeded or was skipped, even while `macos` is still queued or after it failed.
   If any other job failed, was cancelled or timed out, the release fails with an error naming those jobs and the `ci`
   run, and nothing is released. It holds `actions: read` and nothing else, and checks out no code.
   A release therefore does not wait for macOS `ci`: that job queues for hours on the hosted macOS runners, so
   main's own macOS run is the backstop (a failure there is fixed forward), and the release's own `aarch64-apple-darwin`
   build still runs on a Mac (below).
3. `plan` (`cargo xtask release plan`) decides whether to release and as which version (below).
   When there is nothing to release, the run ends there, green.
4. The `artifacts` job calls `.github/workflows/artifacts.yml` with that version; its jobs run in
   the same run, at the same commit. Its `build` stamps the version into the CI checkout, builds the binaries for
   `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (`js-lease-server`, `js`, `js-agent`, `js-bundle`, `js-mdm`, for each of the two Linux architectures) and `aarch64-apple-darwin` (`js`, `js-agent`, `js-bundle`, `js-mac-session`, `js-mac-provision`), each target on a GitHub-hosted runner of its own architecture, checks that
   the Linux ones are static and that `--version` prints the release version, and packages them:
   `cargo xtask release stage` lays down the files of each archive (`crates/xtask/src/release/stage.rs`
   lists them) and `tar` makes it. Each of the two Linux targets also packages the `js-agent` it
   built as a Debian package with `cargo xtask release deb` (below), which `dpkg-deb` then reads,
   and builds the `js-lease-server` it built into a container image with `cargo xtask release image`
   (below). `sums` writes one `SHA256SUMS` over the archives, the two Debian packages and the two images.
5. artifacts.yml's `attest` attests every archive and Debian package with build provenance
   (`actions/attest-build-provenance`, from `SHA256SUMS`), after refusing sums other than the ones
   `sums` made and files that differ from them; it then attests each Debian package and each image
   alone and keeps the bundle of that attestation beside it as `<file>.sigstore.json`. Its `verify`
   then checks the attestations the way a user does (below), and each Debian package and each image
   from its bundle. The
   signer of every attestation is `.github/workflows/artifacts.yml`, not `release.yml`: the apt jobs
   below publish a package only after checking its attestation by that file.
6. `publish` creates the GitHub release for `vX.Y.Z` as a draft (its tag name `vX.Y.Z`, its target the
   commit), uploads the archives, the Debian packages, the images, their bundles and `SHA256SUMS` to
   it, and publishes it in one call. Publishing creates the tag `vX.Y.Z` on the commit. It refuses a
   Debian package or an image without its bundle.

The tag is made last, by that one call, so a run that fails earlier leaves no tag and no public
release: at worst a draft, which only people who can write to the repository see. Re-run the failed
jobs from the run's page (below): the run continues the draft of the version, adds the files it
lacks, and replaces none of a published release (a differing file there fails the run; a stale or
differing draft is replaced).

The run releases its own commit even if `main` has moved on while it waited or built: the attestation
records the commit the run is at, which is the pushed commit, so the files and the provenance agree.
`plan` refuses a commit that a later `v*` tag is past (a tag at a descendant of the commit): a later
release covers it, the version stays monotonic, and no tag lands on an older commit. `publish` checks
this again right before the publishing call: the latest `v*` tag, by version, must be a proper ancestor
of the commit (the compare API says `ahead`), or nothing is published. That closes the race where two
runs finish out of order. Releases are one at a time and none is cancelled: while one runs, GitHub keeps
one waiting run and a newer push replaces an older waiting one, whose change the newer release covers.

The two checks read tags differently, on purpose. `plan` looks only at `v*` tags merged into the commit;
`publish` compares against the highest `vX.Y.Z` tag of the whole repository, so it fails closed: a `v` tag
that is not on `main` (pushed by hand, or left on a side branch) blocks every release until it is removed
(see Recovery). A push to `main` of a fork runs this workflow in the fork, with the fork's own token, and
`gamma` stays off there without its variables.

The archives are made to be byte-identical when built again from the same commit: sorted file names,
the commit's time as modification time, numeric owner 0, `gzip -n`. The packages are too: their tar
headers carry no owner, name or time, and the zstd frame is a fixed encoder's output. A full re-run therefore uploads
the same files the first run did; if a build still came out different, the run fails at the
comparison rather than mix two builds (see Recovery).

A gap to know: a waiting run replaced by a newer push is made up by the newer push's run. If that newer
push fails `ci` (or its release run fails), nothing releases the replaced change until the next push
that passes `ci` and ships code. Nothing runs on a timer.

7. The apt jobs (off until their settings exist, below) publish the two Debian packages into the
   suite `testing` of the apt repository at `https://packages.komira.ai/apt`; `check-apt` installs
   `js-agent` from that suite in a clean Ubuntu container on an amd64 and an arm64 runner; only when
   both installs passed are the same packages promoted into the suite `stable`
   ([The apt repository](#the-apt-repository)).
8. `gamma` (off until its variables are set, below) sets the release as the desired version of the
   service `js-server` on the update coordinator and waits for it to converge.

After the run, `.github/workflows/hostkeeper-desired.yml` (off until its variable is set) sets the
release as hostkeeper's desired version of `js-agent` ([Deploying through
hostkeeper](#deploying-through-hostkeeper)).

Each job holds only the permission it needs: `contents: read` for plan, build, deploy-tool, sums,
verify, the apt index jobs and `check-apt` (plus `attestations: read` for verify), `id-token` and
`attestations` write for `attest`, `actions: read` alone for `ci-passed`, `contents` write for
`publish` (the tag and the release), `contents: read` and `id-token` write for the apt read and
write jobs (the token is exchanged for the apt publish role), `id-token` write alone for `gamma`.
The `artifacts` job of release.yml grants `contents: read` and the two writes of `attest`, since a
called workflow's jobs can hold no more than the calling job grants; each job of artifacts.yml then
asks for its own. No job
that holds a write permission or the signing key compiles code: `gamma` and the apt read, index and
write jobs run the `xtask` binary that `deploy-tool` built in a job with no token and no secret, so
the build, whose scripts run code from the locked dependency set, never shares a job with the token
service's address, the cloud role or the key. The rules (SHA-pinned
actions, no permission at the top, `id-token` write in `attest`, the apt read and write jobs and
`gamma` only, signing in artifacts.yml's `attest` only) are pinned by
`crates/xtask/tests/release_workflow`, and the apt jobs by `crates/xtask/tests/apt_workflow`.

## What is released

A push is released when a file that ships changed since the latest release tag. The set is computed
from the dependency graph (`cargo metadata`), not listed: the crates of `js-lease-server`, `js-cli`
(the `js` binary), `js-agent`, `js-bundle-cli` (the `js-bundle` binary), `js-mac-session`, `js-mac-provision` and `js-mdm`, and every workspace crate they depend on through normal or build
dependencies. Every file of those crates ships (`src/**`, `Cargo.toml`, `build.rs`, a sample
configuration compiled in with `include_str!`) except `tests/`, `benches/`, `examples/` and markdown;
so do the root `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` and everything under `.cargo/`,
`packaging/js-lease-server/updater.d/js-lease-server.toml`, the host updater's declaration, which the
Linux `js-lease-server` archives carry as they are, and the files the Debian package of `js-agent`
is made of, which `release deb` compiles in: those under `packaging/js-agent/deb/` (markdown aside)
and `packaging/js-agent/js-agent.service`, `packaging/js-agent/tmpfiles.conf` and
`packaging/js-agent/crun_host_setup.sh` (`DEB_DIRS` and `DEB_FILES` in `crates/xtask/src/release.rs`).

A file in one of the excluded places still ships when a shipped source compiles it in. `plan` reads
the `include_str!`, `include_bytes!` and `include!` calls of every shipped `.rs` file (not those
under `tests/`, `benches/` or `examples/`), resolves each path from the source file, and counts that
file as shipped wherever it is: an `include_str!("../help.md")` makes `help.md` release. The path must
be a plain string literal; `concat!`, `env!`, a raw string or an escape in an include fails the plan
(the run goes red) rather than guess what is compiled in.

A change that moves a shipped file out of a shipped directory, or deletes one, releases: the old path
is a change of the crate.

These do not release: `docs/**` and every `*.md`, `.github/**`, a crate's `tests/`, benches and
examples, and the crates that no shipped binary depends on (`xtask`, `js-coverage`,
`js-conformance`). A push that changes only those ends the run at `plan`. A later push that does
ship releases everything since the latest tag, including code an earlier push left unreleased.

## Which version

The next version is the latest `vX.Y.Z` tag (by version order) that is merged into the commit, with
its patch number raised by one. A higher tag that is not an ancestor of the commit (on another
branch, or pushed by mistake) is not a release of this history and is ignored. With no tag yet it is
the workspace version.

To release a minor or major version, merge a pull request that raises the workspace version in
`Cargo.toml` (`[workspace.package] version`) above the latest tag, for example to `0.2.0`. The next
release that ships code uses that version as it is (the pull request changes `Cargo.toml`, so it
ships and releases itself), and the releases after it count patches from it: `0.2.1`, `0.2.2`. A
workspace version at or below the latest tag changes nothing, so the version in `Cargo.toml` need
not be kept up to date for patch releases.

The release version is not committed. `build` runs `cargo xtask release stamp --to X.Y.Z` in its
checkout, which sets the `[workspace.package] version` in `Cargo.toml` and the version of each
workspace package in `Cargo.lock`, so `cargo build --locked` accepts the files and every binary prints
the release version with `--version`. A build from the repository itself prints the workspace version.

## Assets

For version `X.Y.Z`:

| Asset | Contents |
|---|---|
| `js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js-lease-server` binary, `LICENSE` and `updater.d/js-lease-server.toml`, the host updater's declaration (below) |
| `js-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js` binary and `LICENSE` |
| `js-agent-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js-agent` binary and `LICENSE` |
| `js-X.Y.Z-aarch64-apple-darwin.tar.gz` | the `js` binary for Apple silicon Macs and `LICENSE` |
| `js-agent-X.Y.Z-aarch64-apple-darwin.tar.gz` | the `js-agent` binary for Apple silicon Macs and `LICENSE` |
| `js-mdm-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js-mdm` binary and `LICENSE` |
| `js-bundle-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js-bundle` binary (packs, prints and verifies darwin bundles) and `LICENSE` |
| `js-bundle-X.Y.Z-aarch64-apple-darwin.tar.gz` | the `js-bundle` binary for Apple silicon Macs and `LICENSE` |
| `js-lease-server-X.Y.Z-aarch64-unknown-linux-musl.tar.gz` | the `js-lease-server` binary for arm64 Linux hosts, `LICENSE` and `updater.d/js-lease-server.toml`, as in the x86_64 archive |
| `js-X.Y.Z-aarch64-unknown-linux-musl.tar.gz` | the `js` binary for arm64 Linux hosts and `LICENSE` |
| `js-agent-X.Y.Z-aarch64-unknown-linux-musl.tar.gz` | the `js-agent` binary for arm64 Linux hosts and `LICENSE` |
| `js-bundle-X.Y.Z-aarch64-unknown-linux-musl.tar.gz` | the `js-bundle` binary for arm64 Linux hosts and `LICENSE` |
| `js-mdm-X.Y.Z-aarch64-unknown-linux-musl.tar.gz` | the `js-mdm` binary for arm64 Linux hosts and `LICENSE` |
| `js-mac-session-X.Y.Z-aarch64-apple-darwin.tar.gz` | the `js-mac-session` root helper for Apple silicon Macs and `LICENSE` |
| `js-mac-provision-X.Y.Z-aarch64-apple-darwin.tar.gz` | the `js-mac-provision` pf anchor installer for Apple silicon Macs and `LICENSE` |
| `js-agent_X.Y.Z_amd64.deb` | the Debian package of `js-agent` for x86_64 Linux hosts (below) |
| `js-agent_X.Y.Z_arm64.deb` | the Debian package of `js-agent` for arm64 Linux hosts |
| `js-agent_X.Y.Z_<arch>.deb.sigstore.json` | for each of the two Debian packages, the Sigstore bundle of its own build provenance attestation |
| `js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.oci.tar` | the `js-lease-server` container image for x86_64 Linux hosts, an OCI image layout that `docker load` reads (below) |
| `js-lease-server-X.Y.Z-aarch64-unknown-linux-musl.oci.tar` | the same image for arm64 Linux hosts |
| `js-lease-server-X.Y.Z-<target>.oci.tar.sigstore.json` | for each of the two images, the Sigstore bundle of its own build provenance attestation |
| `SHA256SUMS` | SHA-256 of the fifteen archives, the two Debian packages and the two images (not of the bundles) |

Each archive unpacks to a directory named like the archive without `.tar.gz`.

The Linux `js-lease-server` archives, of both architectures, also hold `updater.d/js-lease-server.toml`,
the declaration the host updater loads the hook's rights from
(`packaging/js-lease-server/updater.d/js-lease-server.toml`; the updater's own install step puts it
in `/etc/updater.d` from a verified release, after checking it asks for no more than the approved
one). It sits beside the binary, at the name kbf's release unpacker takes a declaration by
(`updater.d/<binary>.toml`), and the attestation of the archive covers it. Its `signer` is this
repository's `artifacts.yml`, the workflow that signs every release file. A host whose
`/etc/updater.d/js-lease-server.toml` still names `release.yml` refuses the releases signed by
`artifacts.yml`: set that line to `workflow = "artifacts.yml"` on the host.
`crates/xtask/tests/js_lease_server_declaration.rs` reads it as the declaration's shape and with the
hook's own reader, and `crates/xtask/tests/release_stage.rs` fails if the archive leaves it out.
What the server's hook does is in [lease-server-update.md](lease-server-update.md). The `js-agent`
archives carry no declaration: the agent is updated through hostkeeper (below).

### The Debian package of js-agent

`js-agent` is a hostkeeper service, named `js-agent`, installed on a Linux host from its Debian
package by hostkeeper's `apt` installer. `build` makes the package of each Linux architecture from
the binary it just built, with `cargo xtask release deb --binary <path> --version X.Y.Z --platform
linux-x86_64|linux-arm64 --out dist` (`crates/xtask/src/release/deb.rs`), and `dpkg-deb` reads its
control and its files back:

| File | Mode | From |
|---|---|---|
| `/usr/bin/js-agent` | 0755 | the release binary, byte for byte |
| `/usr/lib/systemd/system/js-agent-crun.service` | 0644 | `packaging/js-agent/deb/js-agent-crun.service`, the agent run with crun |
| `/usr/lib/systemd/system/js-agent-crun-gpu.service` | 0644 | `packaging/js-agent/deb/js-agent-crun-gpu.service`, the oneshot that writes the GPU drop-in |
| `/usr/lib/tmpfiles.d/js-agent-crun.conf` | 0644 | `packaging/js-agent/deb/tmpfiles.conf`, the directories of the unit |
| `/usr/lib/sysusers.d/js-agent.conf` | 0644 | `packaging/js-agent/deb/sysusers.conf`, the job user `jsjob` of the unit |
| `/usr/lib/systemd/system/js-agent.service` | 0644 | `packaging/js-agent/js-agent.service`, the agent run through Docker or Podman on a host without hostkeeper |
| `/usr/lib/tmpfiles.d/js-agent.conf` | 0644 | `packaging/js-agent/tmpfiles.conf`, the directories of that unit |
| `/usr/share/js-agent/crun_host_setup.sh` | 0755 | `packaging/js-agent/crun_host_setup.sh`, the held host setup script |
| `/usr/share/doc/js-agent/copyright` | 0644 | `LICENSE` |

The package is `js-agent`, version `X.Y.Z`, architecture `amd64` or `arm64`. Its one dependency,
`systemd | systemd-standalone-sysusers` (the binary is static), provides the `systemd-sysusers` its
one maintainer script, `postinst` (`packaging/js-agent/deb/postinst`), runs to make the job user
from the package's sysusers file and to give it a subordinate uid and gid range where it has none
(`packaging/js-agent/deb/README.md`); nothing enables or starts its units. hostkeeper runs
`js-agent-crun.service` (the manifest's `run` unit, below); `js-agent.service` is the unit of a host
that installs the package by hand and starts it as that unit's header says. The bytes depend
only on the binary and the packaging files: root-owned entries, a fixed modification time, sorted
names, gzip without a name. The verb refuses a version that is not `X.Y.Z`, a binary that does not
carry the version's `--version` line, and a file that is already there. The package carries no
server address, pool, token or host name: the host's own flags are in `/etc/js-agent/flags` (read
with `--flags-file`) and its join token in `/etc/js-agent/token`, which an operator places
(`packaging/js-agent/deb/README.md`).

The `.deb` replaces the hostkeeper archive package of earlier releases (`js-agent-X.Y.Z-<platform>.tar.zst`
and `js-agent-index.json`): hostkeeper installs native packages only. The pull-request check
`.github/workflows/js-agent-deb.yml` builds the x86_64 package the same way on every pull request
that changes a crate, the lock file, the toolchain or the packaging, and publishes nothing.

### The js-lease-server image

`build` makes an image of the `js-lease-server` it built for each Linux target, with
`cargo xtask release image --binary <path> --version X.Y.Z --target
x86_64-unknown-linux-musl|aarch64-unknown-linux-musl --out dist`
(`crates/xtask/src/release/server_image.rs`). The file is an OCI image layout in a tar: `oci-layout`,
`index.json`, the blobs under `blobs/sha256/` (the manifest, the config and one gzip layer) and a
`manifest.json` that engines older than the OCI layout read, which names the same config and layer.
`docker load -i` reads it; so does anything that reads an OCI layout from a directory once the tar is
unpacked.

The layer holds two files and the directories above them, all owned by `root`:

| File | Mode | From |
|---|---|---|
| `/usr/local/bin/js-lease-server` | 0755 | the release binary, byte for byte |
| `/usr/share/doc/js-lease-server/LICENSE` | 0644 | `LICENSE` |

Nothing else is in the image: no shell, no libraries (the binary is static musl) and no certificate
bundle. The server does not read the system's certificates. Its calls to the cloud providers' APIs and
the Tailscale API trust the public roots built into the binary, so they need nothing mounted; the
root certificate of the state store's TLS (`verify-ca` and `verify-full`) is the file the store URL
names with `sslrootcert`, which the operator mounts into the container.

The config runs `/usr/local/bin/js-lease-server` as the numeric user and group `65532:65532` (the
image has no `/etc/passwd`) from `/`, with `SIGTERM` as the stop
signal, and gives the program no command: the server's flags follow the image in `docker run`. It
exposes no port, because the server listens where its flags say (`--listen` is the loopback by
default, and a listener off the loopback needs TLS), and declares no volume. The labels are
`org.opencontainers.image.version` (`X.Y.Z`), `org.opencontainers.image.source` (this repository) and
`org.opencontainers.image.licenses` (`Apache-2.0`). The config has no creation time, so the bytes depend
only on the binary, the version and the target: root-owned entries, a fixed modification time, sorted
names, gzip without a name or a time. The verb refuses a target other than the two, a version that is
not `X.Y.Z`, a binary that does not hold the version as text, and a file that is already there. It
prints the file's SHA-256 and size and the image's digest.

The image has no name and no tag. It is identified by its digest: the `sha256` of the manifest that
`index.json` names. Read it from the tar, or from the line `release image` printed:

```sh
tar -xOf js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.oci.tar index.json
```

`manifests[0].digest` is `sha256:<hex>`; `manifests[0].platform` says `linux/amd64` or `linux/arm64`. On
an engine that uses the containerd image store, `docker load` reports that same digest as the image
ID. Deploy by that digest, not by a tag the image does not have.

#### Running the image

What a deployer gives the container, and what it can expect back. The server's flags, with their
defaults and refusals, are documented in `crates/js-lease-server/src/lib.rs` ("Flags"); this is
what changes when the server runs in a container.

**Command.** The entrypoint is `/usr/local/bin/js-lease-server` and the image has no command, so
everything after the image in `docker run` is the server's flags: either the flags themselves or
`--config <path>`, the one file that replaces them. The server's own required settings apply
(`--boot`, `--state-file`, `--token-file`, `--agent-pool-config` or the file's `[[pool]]`, and
`--store` where the state is kept in a store). The image adds no flag and no environment variable.

**Network and ports.** The image exposes no port: each listener is where its flag puts it.

| Flag | Default | Serves | Note for a container |
|---|---|---|---|
| `--listen` | `127.0.0.1:7300` | the API, the agents, `GET /healthz` | an address off the loopback needs `--tls-cert` and `--tls-key`. With the default, run on the host's network or nothing outside the container reaches it |
| `--job-listen` | off | the job service | same TLS rule |
| `--metrics-listen` | off | `GET /metrics` | a private address |
| `--ready-listen` | off | `GET /readyz` | the loopback only: the probe must run in the container's network namespace (the host's network, or a sidecar in the same namespace). The image has no shell and no HTTP client, so an engine `HEALTHCHECK` that runs a command inside it cannot be used |

**Files and volumes.** The image declares no volume and runs as `65532:65532`. Mount:

| What | Mode in the container | Why |
|---|---|---|
| the directory of `--state-file` | read-write, owned by `65532` | the server writes `<path>.agents`, `<path>.joins`, `<path>.scaler`, `<path>.jobs` and, unless `--usage-db` puts it elsewhere, `usage.db` beside them. This is the state that must outlive the container |
| `--usage-db`'s directory, when it is not that one | read-write, owned by `65532` | the usage file |
| `--token-file`, `--client-token` files, `--agent-token-file`, `--store`, `--tls-key` | read-only, owned by `65532`, mode 0600 or 0400 | the server refuses a secret file that is not owner-only. For `--tls-key` a symbolic link, as secret mounts make, is followed and the file it leads to is checked |
| `--agent-pool-config` or `--config`, `--tls-cert` | read-only | |
| the store's `sslrootcert`, or the directory of a Postgres Unix socket | read-only | the store URL names them |

**Readiness and health.** Two routes answer two questions:

- `GET /readyz` on `--ready-listen`: 503 `waiting for the store's writer lock` while another process
  holds it, 503 `starting` until every listener serves, 200 `ready`, then 503 `draining` from SIGTERM
  until the process exits. Route traffic to a container only while it answers 200. With
  `--ready-listen` and a store, a server whose store's writer lock is held waits for it instead of
  exiting, so a replacement can be started before the old process has let the lock go.
- `GET /healthz` on `--listen`: 200 `ok` while the tick loop runs; 503 `tick loop stopped`, or 503
  `store unreachable` once the store paths have failed for `--store-unreachable-after`. It says
  nothing about the writer lock.

**Stopping.** The stop signal is `SIGTERM` (ctrl-c's `SIGINT` does the same). The server answers
`/readyz` 503 `draining` at once, stops accepting, gives the requests in flight `--drain-timeout`
(`10s` by default) to finish, cuts what is still open, ticks once more and exits 0. Give the engine a
stop timeout longer than `--drain-timeout` plus that last tick, for example
`docker stop --time 30` with the default drain, so the engine's `SIGKILL` never cuts the drain. A
signal while the server waits for the lock or starts ends it at once with status 0, without taking
the lock.

**Exit status.** 0 after a stop. 2 when the start is refused: a bad flag or file, the store
unreachable, or (without `--ready-listen`) another process holding the store's writer lock. Do not
restart a container in a loop on 2: the same flags are refused again. 1 when the server stops on its
own while running, such as when it loses the store's writer lock.

**One writer per store.** Two servers never serve one store at once: the writer lock lets one in.
With `--ready-listen`, a second server waits and answers 503 until the first lets the lock go.

An example, with the server on the host's network, its state in `/var/lib/js-lease-server` and its
flags in a file under `/etc/js-lease-server`, run by the image ID `docker load` printed (the manifest's digest on an engine with the containerd image store):

```sh
docker load -i js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.oci.tar
docker run --name js-lease-server --network host --stop-timeout 30 \
  -v /var/lib/js-lease-server:/var/lib/js-lease-server \
  -v /etc/js-lease-server:/etc/js-lease-server:ro \
  sha256:<image id> \
  --config /etc/js-lease-server/server.toml
```

with the `[server]` table of `server.toml` setting `state_file = "/var/lib/js-lease-server/state"`,
`ready_listen = "127.0.0.1:<port>"` and the rest of the server's settings.

### The apt repository

The packages are published into the shared apt repository at `https://packages.komira.ai/apt` (an S3
bucket behind a CDN; component `main`, architectures `amd64` and `arm64`), in two suites over one
pool: `testing` first, then `stable`. The pool path of this repository's packages is
`pool/jobscheduler/`, the one prefix its publish role may write to (besides the suites' indexes). The
jobs are tierstore's (`crates/xtask/src/channels`, `cargo xtask channels ...`); they keep the entries
of every other product of the repository as they are.

| Job | Environment | What it does |
|---|---|---|
| `testing-apt-read` | `apt-publish` | assumes the publish role through the job's OIDC token (`channels assume`) and reads the suite's index and the ETag of each file from the bucket (`channels apt read`) |
| `testing-apt-index` | `apt-signing` | checks the two packages against `SHA256SUMS` and their attestation by artifacts.yml at this commit, merges them into the index read (`channels apt index`) and signs `Release` (`InRelease`, `Release.gpg`) with the repository's subkey (`channels apt sign`), which it then removes |
| `testing-apt-write` | `apt-publish` | checks the packages again, uploads each once to the pool (never over other bytes), writes the signed index with `If-Match` on the ETags read, and invalidates `/apt/dists/*` on the CDN (`channels apt write`) |
| `check-apt` | none | on an amd64 and an arm64 runner, in a clean Ubuntu container pinned by digest: adds the repository with its keyring, installs `js-agent=X.Y.Z` from `testing`, and checks the version, the files, that the agent's own start check accepts the job user `jsjob` (a system account with no home and no login) and its subordinate uid and gid ranges, that nothing is enabled and that nothing runs (`channels check-apt`) |
| `stable-apt-read`, `stable-apt-index`, `stable-apt-write` | as above | only when `check-apt` passed on both: read both suites, list in `stable` the entries `testing` holds for this version, held to the digests of `SHA256SUMS` (`channels apt promote`), sign, and write the index alone (`--no-pool`): `stable` serves the very bytes `testing` was checked with |

No job holds both the signing key and the cloud role. A write that loses the race for the shared
index to another product's release (an `If-Match` that fails) says so (`conflict=true`) and the next
attempt reads, merges, signs and writes again: each suite has three attempts (`-2` and `-3` jobs);
the third fails the run if it loses too. Each read, index and write job first runs `channels gate`
on its settings: with none of them set it does nothing and passes (a notice), with some of them set
it fails naming the missing ones. With nothing written to `testing`, `check-apt` and the promotion to
`stable` do not run.

What the project owner sets (Settings, Environments; both restricted to `main`):

| Environment | Variables | Secrets |
|---|---|---|
| `apt-publish` | `APT_PUBLISH_ROLE_ARN` (the role this repository's release assumes; it may write `apt/pool/jobscheduler/` and the suites), `APT_BUCKET`, `APT_DISTRIBUTION_ID` (the CDN distribution), `APT_HOSTNAME` (`packages.komira.ai`) | none |
| `apt-signing` | `APT_GPG_FINGERPRINT` (the signing subkey; several, comma-separated, during a rotation) | `APT_GPG_PRIVATE_KEY`, `APT_GPG_PASSPHRASE` |

To install the agent by hand from the repository:

```sh
curl -fsSL https://packages.komira.ai/apt/komira-archive-keyring.gpg -o /usr/share/keyrings/komira-archive-keyring.gpg
cat > /etc/apt/sources.list.d/komira.sources <<'SOURCES'
Types: deb
URIs: https://packages.komira.ai/apt
Suites: stable
Components: main
Signed-By: /usr/share/keyrings/komira-archive-keyring.gpg
SOURCES
apt-get update && apt-get install js-agent
```

### Deploying through hostkeeper

`.github/workflows/hostkeeper-desired.yml` sets the desired version of the hostkeeper service
`js-agent` on hostkeeper's coordinator, through hostkeeper's reusable workflow `set-desired.yml`
pinned at a hostkeeper release (its `hkctl` is downloaded and its attestation checked before any
step holds a token). It starts after every run of `release` on `main` that succeeded (it acts when
that run's `publish` succeeded, on the newest published release), or by hand from `main` with a
version (a rollback may name an older one). Its `check` job (`cargo xtask release desired`,
`crates/xtask/src/release/desired.rs`) refuses a release unless the suite `stable` lists both of
its Debian packages, read through the CDN as a host reads it, with the digests of its `SHA256SUMS`,
and writes the release's targets: for each of `linux-x86_64` and `linux-arm64` one `apt` entry
naming the host's repository entry `komira-stable`, the Debian version `X.Y.Z`, the package's
SHA-256 as the pin, and the manifest:

| Manifest field | Value |
|---|---|
| `package` | `js-agent` |
| `hook.command` | `/usr/bin/js-agent update-hook` (hostkeeper runs it from the staged copy of the package) |
| `hook.timeoutSecs` | prepare 3900, cutover 4320, verify 615, rollback 8220, cleanup 120; no uninstall: hostkeeper refuses an uninstall order of the agent |
| `hook.verifyDeadlineSecs`, `hook.cleanupRetries` | 9000, 3 |
| `run` | `js-agent-crun.service` |
| `params` | the inputs of `js-agent update-hook` (`js_agent::update_hook::params::KEYS`): the unit, the hook's state directory, hostkeeper's `current` link, the flags and token files, the state root, the runtime, the cordon and drain bounds and the step bounds |
| `minHostkeeper` | `0.1.225`, the first hostkeeper whose daemon runs a native entry's hook command |
| `installerBounds` | stage 600, activate 300, remove 300 |

There is no `macos-arm64` entry (below). The set does not wait for the fleet: the coordinator rolls
the version out by its own plan.

**Which hosts.** Every deployment of hostkeeper targets a host group (hostkeeper's
`docs/design/hostgroups.md`), and the group a call names is an input of `set-desired.yml` that
hostkeeper has not built yet (HostGroups, HG7). Until it is, the call names only the service, and the
coordinator's current placement decides the hosts: its rollout file's `[[service]]` entry for
`js-agent` (its roles) and the hosts assigned the service. When HG7 lands, the call gains the
`host_group` input and the coordinator's allow-list entry its `host_groups`.

**It is off by default.** Until the repository variable `JS_AGENT_SETS_DESIRED` is `true`, exactly,
both jobs are skipped. Its other settings are repository variables, none of them secret:

| Variable | Meaning |
|---|---|
| `JS_AGENT_SETS_DESIRED` | `true` turns the workflow on |
| `HOSTKEEPER_COORDINATOR_URL` | the coordinator's private listener, `https://<host>[:<port>]` |
| `HOSTKEEPER_COORDINATOR_CA` | the PEM certificates of the CA that listener chains to |
| `HOSTKEEPER_TAILNET_CLIENT_ID` | the client ID of the tailnet's federated identity for this repository; empty: the runner does not join the tailnet |
| `HOSTKEEPER_TAILNET_AUDIENCE` | the audience of that federated identity |
| `HOSTKEEPER_TAILNET_COORDINATOR` | the coordinator's name on the tailnet, which the join pings |
| `HOSTKEEPER_TAILNET_TAG` | the tag the join gives the runner's node, the one that identity issues (such as `tag:ci`) |

The coordinator lets the job's OIDC token set the version only through an entry of its allow-list
(`[[remote_operator]]` of its `--oidc-config`) for this repository:

```toml
[[remote_operator]]
issuer = "https://token.actions.githubusercontent.com"
audience = "jobscheduler-hostkeeper-desired"
repository_id = "<gh api repos/komira-ai/jobscheduler --jq .id>"
repository = "komira-ai/jobscheduler"
job_workflow_ref = ["komira-ai/hostkeeper/.github/workflows/set-desired.yml@318bcc93c9bcd009d40597c45227278742acc622"]
refs = ["refs/heads/main"]
event_names = ["workflow_run", "workflow_dispatch"]
runner_environment = "github-hosted"
services = ["js-agent"]
```

`job_workflow_ref` is hostkeeper's `set-desired.yml` at the commit this repository pins, so moving
the pin (a reviewed change of the `uses:` line, `hostkeeper_version` and `hostkeeper_commit`
together) moves the entry. The coordinator's rollout file needs a `[[service]]` entry for
`js-agent`, and each host the `komira-stable` repository entry.

### macOS

The Mac has no native package hostkeeper can install yet, so no `macos-arm64` target: a Mac whose
daemon announces only `macos-pkg` is ordered nothing for `js-agent`. The signed installer package
(`.github/workflows/js-agent-macos-pkg.yml`, `packaging/js-agent-macos`) is not ready for hostkeeper's
`macos-pkg` installer. What is missing: a release job that builds, signs and attests the `.pkg` (the
workflow runs by hand only); `productsign --timestamp` and hostkeeper's signature check of the signed
package (`release check-pkg-signature`: the chain of the Developer ID Installer identity to Apple's
root, a trusted timestamp); hosting under `pkg/js-agent/<version>/` at `packages.komira.ai` with a
testing-to-stable promotion, which the publish role may not write today; the `macos-pkg` target
entry (its store, locator, digest and size, and the manifest with the package identifier and launchd
label); a decision on notarization, which changes the bytes after signing; and js-agent's hook reading
the `.pkg`'s tree under `tree/Library/...`. The Mac archives (`*-aarch64-apple-darwin.tar.gz`) are
released as before.

The Linux binaries are statically linked against musl, for both architectures (`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl`), so each runs on any Linux host of its architecture, including `gcr.io/distroless/cc-debian12`. Each Linux target is built natively on a GitHub-hosted runner of its architecture (`ubuntu-24.04` for x86_64, `ubuntu-24.04-arm` for arm64), with no cross linker and no emulation, so the `--version` check runs every binary on a host of the kind it is for, and the static check (no `NEEDED` entry in `readelf -d`) is the same for both. The TLS stack (rustls with ring) needs a C compiler for ring, which the workflow gets from `musl-tools` on both Linux runners, its packages pinned by version and SHA-256 in `.github/debs/musl-tools.toml` and installed with `dpkg` after `cargo xtask debs` took them from the actions cache or, when the cache missed, from the pinned mirrors (on the arm64 runner a link gives its `musl-gcc` the name `aarch64-linux-musl-gcc`, the one the `cc` crate looks for there); musl itself needed no crate code change (the Mac build needed one: `js` draws its run label and `js-agent` its retry jitter through the `getrandom` crate, which both targets share). The macOS assets (`aarch64-apple-darwin`) are built on a `macos-15` runner with `MACOSX_DEPLOYMENT_TARGET=11.0` pinned (the build fails if a binary's minimum macOS is above it) and are dynamically linked against the system libraries, as every Mac binary is; there is no macOS `js-lease-server`. `js-mac-session` and `js-mac-provision` are the host tools a Mac node installs; `js-mac-pool` is a library and `js-mac-probe` runs only on hosted CI runners, so neither is shipped. The darwin binaries, including the root helper `js-mac-session`, are not codesigned or notarized. The Mac agent refuses `--container-bin`, whose post-job checks read `/proc`.

Not yet proven: that the Mac archives come out byte-identical when a partly published release is re-run, nor that the Linux arm64 ones do when they are built again on a hosted arm64 runner. The Linux archives of both architectures are held to this by the comparison in `publish`, as every archive is; for the Mac ones and the Linux arm64 ones it is watched on the first real release. `crates/xtask/tests/release_archive_compat.rs` checks the archiving half for the arm64 agent archive: the workflow's `tar` options and `gzip -n` make the same bytes from the same files, whatever the working directory and the files' own times.

## Downloading and verifying

Needs the GitHub CLI (`gh`), signed in.

```sh
gh release download vX.Y.Z -R komira-ai/jobscheduler -p 'js-lease-server-*-x86_64-unknown-linux-musl.tar.gz' -p SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
gh attestation verify js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.tar.gz -R komira-ai/jobscheduler \
  --signer-workflow komira-ai/jobscheduler/.github/workflows/artifacts.yml \
  --source-ref refs/heads/main \
  --source-digest "$(git rev-parse vX.Y.Z^{commit})"
```

`git rev-parse` needs a clone with the tags; without one, the commit is the one the release page shows
for the tag. For the client, use `-p 'js-[0-9]*-x86_64-unknown-linux-musl.tar.gz'` instead
(`js-[0-9]*` keeps the other archives out of the match), and for the agent
`-p 'js-agent-*-x86_64-unknown-linux-musl.tar.gz'` (the Debian package: `-p 'js-agent_*_amd64.deb'`), and for the bundle tool `-p 'js-bundle-*-x86_64-unknown-linux-musl.tar.gz'`. The server archive pattern `js-lease-server-*-x86_64-unknown-linux-musl.tar.gz` leaves the image out; the image has its own, below.

On an arm64 Linux host, write `aarch64-unknown-linux-musl` where these say `x86_64-unknown-linux-musl`,
in the pattern and in the file name: for the agent `-p 'js-agent-*-aarch64-unknown-linux-musl.tar.gz'`
and `gh attestation verify js-agent-X.Y.Z-aarch64-unknown-linux-musl.tar.gz ...`. On either Linux
architecture `uname -m` prints the first part of the target (`x86_64` or `aarch64`), so
`-p "js-agent-*-$(uname -m)-unknown-linux-musl.tar.gz"` picks the host's own.

The checksum check proves the download is intact. The attestation check proves the file was built
from this repository by the `artifacts` workflow (which the `release` workflow calls), from `main`
(`--source-ref`), at exactly that commit (`--source-digest`), which is the commit the tag `vX.Y.Z`
names. A release whose assets have no `.sigstore.json` was signed by `release.yml`: check it with
`--signer-workflow komira-ai/jobscheduler/.github/workflows/release.yml`.

A Debian package can also be checked from its bundle, as the apt jobs do, with
`gh attestation verify js-agent_X.Y.Z_amd64.deb --bundle js-agent_X.Y.Z_amd64.deb.sigstore.json`
and the same flags (download the bundle with `-p 'js-agent_*_amd64.deb.sigstore.json'`).

A `js-lease-server` image is checked from its bundle before it is loaded, and is deployed by its digest:

```sh
gh release download vX.Y.Z -R komira-ai/jobscheduler -p 'js-lease-server-*-x86_64-unknown-linux-musl.oci.tar*' -p SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
gh attestation verify js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.oci.tar \
  --bundle js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.oci.tar.sigstore.json -R komira-ai/jobscheduler \
  --signer-workflow komira-ai/jobscheduler/.github/workflows/artifacts.yml \
  --source-ref refs/heads/main \
  --source-digest "$(git rev-parse vX.Y.Z^{commit})"
docker load -i js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.oci.tar
```

The pattern ends in `.oci.tar*` to take the bundle too, and `--bundle` makes `gh attestation verify`
read the attestation from that file instead of fetching it. The digest to deploy by is in the
tar's `index.json` ([The js-lease-server image](#the-js-lease-server-image)); for arm64 write
`aarch64-unknown-linux-musl`.

## Configuration the project owner must set

Apart from the apt jobs' two environments ([The apt repository](#the-apt-repository)), the
workflow needs no secret and no environment. It does need repository settings that only the
project owner can make. This section says what to set; it does not say what has been set. At the time
of writing the repository has no ruleset and no branch protection, so none of the following holds
until it is set.

- **A tag ruleset for `v*`**: block update and deletion of the tags, and allow creation only for the
  GitHub Actions identity (a bypass for GitHub Actions, and for nobody else). Without it anyone who
  can write to the repository can move or delete a release tag, or push a `v*` tag by hand. A
  hand-pushed tag starts nothing (below), but a tag merged into `main` is the base the next version
  counts from.
- **Branch protection for `main`**: require a pull request. A push to `main` that passes `ci` is a
  release, so whoever can push to `main` can cut one. The workflow does not depend on this, but the
  guarantee that a release is a reviewed change does. Its required status checks include the two
  macOS checks every pull request runs, `macos` (ci.yml) and `js-agent-macos-pkg`
  (js-agent-macos-pkg.yml), so a change that breaks the Mac build or package cannot merge;
  `tools/held/required-checks/require_macos_checks.sh` adds them (a dry run by default,
  `--apply` to change the protection).
- **The environments `apt-publish` and `apt-signing`**, restricted to `main`, with the variables and
  secrets [The apt repository](#the-apt-repository) lists. Until they exist every apt job passes
  doing nothing, and the release ends with the GitHub release published and no package in the
  repository.

Not proven until the first real run on `main`, and fixed there if it differs:

- that creating the tag by publishing the release is allowed under the tag ruleset above;
- the values of the attestation's source claims, `--source-ref refs/heads/main` and
  `--source-digest <commit>`, and the signer `artifacts.yml` (the called workflow, not its
  caller `release.yml`), for a run started by a push (artifacts.yml's `verify` refuses the release
  before anything is published if they differ);
- the draft and asset calls of `publish` (listing drafts, uploading to a draft, reading an asset
  back to compare it).

## The gamma step

After a release is published, `gamma` tells the farm's update coordinator which version of the
lease server is wanted, so the hosts that run js-lease-server update themselves. It runs
`cargo xtask deploy` (`crates/xtask/src/deploy.rs`, which documents the calls) for one service,
`js-server`, the binary `js-lease-server`. js-agent is not set here: it is a hostkeeper service,
set by `hostkeeper-desired.yml` ([Deploying through hostkeeper](#deploying-through-hostkeeper)).
js-lease-server is not deployed by hostkeeper and has no Debian package: the host updater installs
it from its archive with its declaration (above).

The job reads `GET /v1/convergence?service=NAME` (the hosts live now are the baseline),
sets the version with `POST /v1/desired` (`{"service": NAME, "version": V, ...}`), then polls the
convergence every 30 seconds with a fresh OIDC token each time (audience: the coordinator's URL).
It passes when the coordinator says `converged` and every role has at least as many hosts at the
version as were live before. It fails when a host failed its update (`blocked`), when a role lost
hosts, when the counts have not changed for 20 minutes, after 60 minutes, when the token is
refused (401, 403: the coordinator's allow-list must name this repository, this workflow on `main`
and the service), or when another writer replaced the desired version.

**The version text** is the release version `X.Y.Z` of the `plan` job: the text `release stamp`
puts in the build, which each binary prints as the last word of `--version`
(`build` checks it) and which `js-lease-server` reports as its version in the coordinator
heartbeat. It is not the tag (`vX.Y.Z`) and has no commit suffix. It is defined in
one place, `deploy::check_version`, which refuses anything but three numbers without leading
zeros, so a different form fails the job before any call is made, whether or not a coordinator
is set. The workflow passes the plan's `version` output and derives nothing from it.

**It is off by default.** With the repository variable `JS_COORDINATOR_URL` empty or unset the job
prints `::notice::the deploy step is off` and passes. It sets nothing and holds no secret.
Repository variables (Settings, Secrets and variables, Actions, Variables):

| Variable | Meaning |
|---|---|
| `JS_COORDINATOR_URL` | the coordinator's control address, `https://<name>:<port>`; unset: the step is off |
| `JS_COORDINATOR_CA` | the CA certificate (public, PEM text) the coordinator's certificate chains to; unset: the runner's system roots |
| `TS_CLIENT_ID`, `TS_AUDIENCE` | when the coordinator is reachable only on a Tailscale network: the trust credential the job joins it with, as an ephemeral node tagged `tag:ci`, using its own OIDC token; either unset: no join |

The coordinator must already run the service `js-server` and allow this repository's `release`
workflow on `main` to set it; until it does, the calls are refused and
the job fails, so leave `JS_COORDINATOR_URL` unset until then. The job fails the run's last step
only: the release is already published, and "Re-run failed jobs" repeats the step.

## Recovery

Open the failed run on the Actions page and use "Re-run failed jobs", not "Re-run all jobs": the files
that were built and verified are kept and nothing is built twice. Re-running only the failed jobs
keeps the `plan` job that succeeded, with its outputs: `plan` does not look at `main` again. The
guards of such a re-run are in the jobs that run again: `publish` checks the tag before it
publishes (and once more right before the publishing call), and artifacts.yml's `verify` checks the
attestations against the commit. "Re-run all jobs" runs `plan` again, which checks that `main` is still at the
commit and may find nothing to release.

A published release is never changed: its files are never deleted or replaced and its commit never
moves. A draft is not published and has no tag, so `publish` may replace it: a draft that targets
another commit, or holds a file that differs from this run's, is deleted and made again, and a file
whose upload did not finish is deleted before it is uploaded again.

| What happened | State | What to do |
|---|---|---|
| `plan`, `build`, `sums`, `attest` or `verify` failed | No tag, no release | Re-run failed jobs. After `verify` failed nothing was published; fix the cause (a change to the workflow is a new push) or re-run. |
| An apt job of `testing` failed, or all three attempts lost the race | The release is published; the suite holds this version or not | Re-run failed jobs: a re-run of a read job reads again, and a write never puts other bytes in the pool under a name it holds. A package already in the pool is accepted when its bytes are equal. |
| `check-apt` failed | The release and `testing` hold the version; `stable` does not | Nothing is promoted and hosts are not ordered it (`hostkeeper-desired.yml` acts only after a run that succeeded). Fix forward: the next release supersedes it in `testing`. |
| An apt job of `stable` failed | `testing` holds the version, `stable` may not | Re-run failed jobs. |
| `publish` failed before the release was published (an upload, the draft) | A draft release, no tag | Re-run failed jobs. The run finds the draft of the version, deletes a half-uploaded file, adds the files it lacks and publishes it. |
| `main` moved after such a failure and the new push plans the same version | A draft at the older commit | Nothing to do: the new run deletes the stale draft (or one holding a file that differs), makes a new one and publishes it. |
| `publish` says the tag appeared and names another commit | A draft, nothing published, a tag on the wrong commit | The tag was pushed by hand. Check that the commit is not on `main` (`git merge-base --is-ancestor <commit> origin/main` says no, exit status 1), delete the tag (`git push origin :refs/tags/vX.Y.Z`; the tag ruleset lets the owner), and re-run failed jobs. If the commit is on `main`, see the row for a release on the wrong commit. |
| `plan` chose a version whose tag exists on a commit that is not on `main`, and `publish` fails at its tag check on every run | A hand-pushed tag, no release | List the commits the tag names (`git ls-remote origin 'refs/tags/v*'`) and check each is not on `main`: `git fetch origin main` then `git merge-base --is-ancestor <commit> origin/main`, which exits 1 for a commit that is not. Delete such a tag (`git push origin :refs/tags/vX.Y.Z`), then re-run failed jobs (or push a change that ships). Never delete a tag whose commit is on `main` or that a published release uses. |
| A published release is on the wrong commit (the tag does not name the commit the files were built from) | A public release that must not be trusted | Its files are not changed and the release is not edited. Mark it so on the release page and in a new issue, and cut the next version by pushing a change that ships; users verify with `--source-digest` (above), which fails for these files at the tag's commit. |
| `publish` failed at the last call or after it | Possibly the tag and a published release | Re-run failed jobs: the tag stays, files the release lacks are added, the tag is checked to name the commit. |
| `publish` says a file differs | The release holds a file that is not this run's | Never delete or replace assets of a published release. Cut a new version: push a change that ships (the next version counts from the tag). |
| The run was green but released nothing | `plan` printed why | a later release exists past the commit, or nothing that ships changed. Nothing to do. |
| "Re-run all jobs" after a successful release | The release is complete | It rebuilds the same files and signs them again; `publish` compares each against the release and keeps them, and keeps the release's bundles without comparing them (each signing writes new bytes for the same file). If a file differs the run fails and nothing is changed. |

Runs that wait: a release run starts with its push and waits for the jobs of `ci` but `macos`; if one fails there, the run fails
at `ci-passed` (red, with the `ci` run's link and the failed jobs) and the change is released by the next push that passes
`ci` and ships code. Re-run the failed jobs after re-running `ci`, if the commit should release itself.

There is no dispatch and no tag trigger: a `v*` tag pushed by hand does not start a release, and
`workflow_dispatch` does not exist. An old run that is re-run with "Re-run all jobs" cannot publish its
old commit under a newer release, because `plan` finds a later tag past it, and `publish` checks the tag order again. A re-run of only the failed jobs does not run `plan`
again; the tag checks in `publish` and the attestation check in artifacts.yml's `verify` are what guard it (the draft
it finds is replaced when it targets another commit). To release by hand there is nothing to do but merge a change that ships.

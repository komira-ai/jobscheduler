# Releasing js-lease-server, js and js-agent

Releases are continuous. Nobody tags, and nothing is run by hand: every push to `main` that
changes code that ships is released as the next version, after `ci` has passed on it.

## What happens on each merge to main

1. `ci` runs on the push, as for any push.
2. When `ci` succeeds, the `release` workflow (`.github/workflows/release.yml`) starts. It acts only
   on a successful `ci` run for a push to `main` of this repository, and works on the commit that
   run passed.
3. `plan` (`cargo xtask release plan`) decides whether to release and as which version (below).
   When there is nothing to release, the run ends there, green.
4. `build` stamps the version into the CI checkout, builds the three binaries for
   `x86_64-unknown-linux-musl`, checks that they are static and that `--version` prints the
   release version, and packages them. `sums` writes one `SHA256SUMS` over the archives.
5. `attest` attests every archive with build provenance (`actions/attest-build-provenance`);
   `verify` then checks the attestations the way a user does (below).
6. `publish` creates the annotated tag `vX.Y.Z` on the commit and the GitHub release with the archives
   and `SHA256SUMS`.

The tag is made last, so a run that fails earlier leaves no tag and no release. Re-run the failed
run from its page: a tag that names the commit is kept, a release that exists gets the files it
lacks, and no file already on a release is replaced (a differing file fails the run). If `main` has
moved on since the commit passed `ci`, the run releases nothing: a build is attested with the commit
the workflow runs at, which is the tip of `main`, and the push that moved it releases this change
too.

Each job holds only the permission it needs: `contents: read` for plan, build, sums and verify (plus
`attestations: read` for verify), `id-token` and `attestations` write for `attest`, `contents`
write for `publish` (the tag and the release). No job that holds a write permission runs repository
code.

## What is released

A push is released when a file that ships changed since the latest release tag. The set is computed
from the dependency graph (`cargo metadata`), not listed: the crates of `js-lease-server`, `js-cli`
(the `js` binary) and `js-agent`, and every workspace crate they depend on through normal or build
dependencies. For those crates, `src/**`, `Cargo.toml` and `build.rs` ship; so do the root
`Cargo.toml`, `Cargo.lock` and `rust-toolchain.toml`.

These do not release: `docs/**` and every `*.md`, `.github/**`, a crate's `tests/`, benches and
examples, and the crates that no shipped binary depends on (`xtask`, `js-coverage`, `js-conformance`)
or that only their tests do (`js-provider-fake`). A push that changes only those ends the run at
`plan`. A later push that does ship releases everything since the latest tag, including code an
earlier push left unreleased.

## Which version

The next version is the latest `vX.Y.Z` tag (by version order) with its patch number raised by one.
With no tag yet it is the workspace version.

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
| `js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js-lease-server` binary and `LICENSE` |
| `js-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js` binary and `LICENSE` |
| `js-agent-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js-agent` binary and `LICENSE` |
| `SHA256SUMS` | SHA-256 of the three archives |

Each archive unpacks to a directory named like the archive without `.tar.gz`.

The Linux binaries are statically linked against musl, so they run on any Linux host of that architecture, including `gcr.io/distroless/cc-debian12`. The TLS stack (rustls with ring) needs a C compiler for ring, which the workflow gets from `musl-tools`; no crate code changes were needed. There is no macOS asset yet: `js` does not build for macOS because `js-cli` draws its run label with the Linux-only `getrandom` of rustix. Add `aarch64-apple-darwin` to the build matrix when it does.

## Downloading and verifying

Needs the GitHub CLI (`gh`), signed in.

```sh
gh release download vX.Y.Z -R komira-ai/jobscheduler -p 'js-lease-server-*-x86_64-unknown-linux-musl.tar.gz' -p SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
gh attestation verify js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.tar.gz -R komira-ai/jobscheduler \
  --signer-workflow komira-ai/jobscheduler/.github/workflows/release.yml \
  --source-digest "$(git rev-parse vX.Y.Z^{commit})"
```

`git rev-parse` needs a clone with the tags; without one, the commit is the one the release page shows
for the tag. For the client, use `-p 'js-[0-9]*-x86_64-unknown-linux-musl.tar.gz'` instead
(`js-[0-9]*` keeps the other archives out of the match), and for the agent
`-p 'js-agent-*-x86_64-unknown-linux-musl.tar.gz'`.

The checksum check proves the download is intact. The attestation check proves the file was built
from this repository by the `release` workflow, from exactly that commit (`--source-digest`), which
is the commit the tag `vX.Y.Z` names.

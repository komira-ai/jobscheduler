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
6. `publish` creates the GitHub release for `vX.Y.Z` as a draft (its tag name `vX.Y.Z`, its target the
   commit), uploads the archives and `SHA256SUMS` to it, and publishes it in one call. Publishing
   creates the tag `vX.Y.Z` on the commit.

The tag is made last, by that one call, so a run that fails earlier leaves no tag and no public
release: at worst a draft, which only people who can write to the repository see. Re-run the failed
jobs from the run's page (below): the run continues the draft of the version, adds the files it
lacks, and replaces none of a published release (a differing file there fails the run; a stale or
differing draft is replaced).

`plan` releases the commit only while `main` is still at it, as the GitHub API reports when the job
runs. If `main` has moved on since the commit passed `ci`, the run releases nothing: a build is
attested with the commit the workflow runs at, which is the tip of `main`, and the push that moved it
releases this change too. The same rule stops a full re-run of an old run: its commit is no longer the tip.
`plan` also refuses a commit that a later `v*` tag is past (a tag at a descendant of the commit).

The archives are made to be byte-identical when built again from the same commit: sorted file names,
the commit's time as modification time, numeric owner 0, `gzip -n`. A full re-run therefore uploads
the same files the first run did; if a build still came out different, the run fails at the
comparison rather than mix two builds (see Recovery).

A gap to know: a run skipped because `main` moved is made up by the newer push's run. If that newer
push fails `ci` (or its release run fails), nothing releases the skipped change until the next push
that passes `ci` and ships code. Nothing runs on a timer.

Each job holds only the permission it needs: `contents: read` for plan, build, sums and verify (plus
`attestations: read` for verify), `id-token` and `attestations` write for `attest`, `contents`
write for `publish` (the tag and the release). No job that holds a write permission runs repository
code.

## What is released

A push is released when a file that ships changed since the latest release tag. The set is computed
from the dependency graph (`cargo metadata`), not listed: the crates of `js-lease-server`, `js-cli`
(the `js` binary) and `js-agent`, and every workspace crate they depend on through normal or build
dependencies. Every file of those crates ships (`src/**`, `Cargo.toml`, `build.rs`, a sample
configuration compiled in with `include_str!`) except `tests/`, `benches/`, `examples/` and markdown;
so do the root `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml` and everything under `.cargo/`.

A file in one of the excluded places still ships when a shipped source compiles it in. `plan` reads
the `include_str!`, `include_bytes!` and `include!` calls of every shipped `.rs` file (not those
under `tests/`, `benches/` or `examples/`), resolves each path from the source file, and counts that
file as shipped wherever it is: an `include_str!("../help.md")` makes `help.md` release. The path must
be a plain string literal; `concat!`, `env!`, a raw string or an escape in an include fails the plan
(the run goes red) rather than guess what is compiled in.

A change that moves a shipped file out of a shipped directory, or deletes one, releases: the old path
is a change of the crate.

These do not release: `docs/**` and every `*.md`, `.github/**`, a crate's `tests/`, benches and
examples, and the crates that no shipped binary depends on (`xtask`, `js-coverage`, `js-conformance`)
or that only their tests do (`js-provider-fake`). A push that changes only those ends the run at
`plan`. A later push that does ship releases everything since the latest tag, including code an
earlier push left unreleased.

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
  --source-ref refs/heads/main \
  --source-digest "$(git rev-parse vX.Y.Z^{commit})"
```

`git rev-parse` needs a clone with the tags; without one, the commit is the one the release page shows
for the tag. For the client, use `-p 'js-[0-9]*-x86_64-unknown-linux-musl.tar.gz'` instead
(`js-[0-9]*` keeps the other archives out of the match), and for the agent
`-p 'js-agent-*-x86_64-unknown-linux-musl.tar.gz'`.

The checksum check proves the download is intact. The attestation check proves the file was built
from this repository by the `release` workflow, from `main` (`--source-ref`), at exactly that commit
(`--source-digest`), which is the commit the tag `vX.Y.Z` names.

## Configuration the project owner must set

The workflow needs no secret and no environment. It does need repository settings that only the
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
  guarantee that a release is a reviewed change does.
- **No environment** is used by `release`, so no environment needs approvers or secrets.

Not proven until the first real run on `main`, and fixed there if it differs:

- that creating the tag by publishing the release is allowed under the tag ruleset above;
- the values of the attestation's source claims, `--source-ref refs/heads/main` and
  `--source-digest <commit>`, for a run started by `workflow_run` (`verify` refuses the release
  before anything is published if they differ);
- the draft and asset calls of `publish` (listing drafts, uploading to a draft, reading an asset
  back to compare it).

## Recovery

Open the failed run on the Actions page and use "Re-run failed jobs", not "Re-run all jobs": the files
that were built and verified are kept and nothing is built twice. Re-running only the failed jobs
keeps the `plan` job that succeeded, with its outputs: `plan` does not look at `main` again. The
guards of such a re-run are in the jobs that run again: `publish` checks the tag before it
publishes (and once more right before the publishing call), and `verify` checks the attestations
against the commit. "Re-run all jobs" runs `plan` again, which checks that `main` is still at the
commit and may find nothing to release.

A published release is never changed: its files are never deleted or replaced and its commit never
moves. A draft is not published and has no tag, so `publish` may replace it: a draft that targets
another commit, or holds a file that differs from this run's, is deleted and made again, and a file
whose upload did not finish is deleted before it is uploaded again.

| What happened | State | What to do |
|---|---|---|
| `plan`, `build`, `sums`, `attest` or `verify` failed | No tag, no release | Re-run failed jobs. After `verify` failed nothing was published; fix the cause (a change to the workflow is a new push) or re-run. |
| `publish` failed before the release was published (an upload, the draft) | A draft release, no tag | Re-run failed jobs. The run finds the draft of the version, deletes a half-uploaded file, adds the files it lacks and publishes it. |
| `main` moved after such a failure and the new push plans the same version | A draft at the older commit | Nothing to do: the new run deletes the stale draft (or one holding a file that differs), makes a new one and publishes it. |
| `publish` says the tag appeared and names another commit | A draft, nothing published, a tag on the wrong commit | The tag was pushed by hand. Check that the commit is not on `main` (`git merge-base --is-ancestor <commit> origin/main` says no, exit status 1), delete the tag (`git push origin :refs/tags/vX.Y.Z`; the tag ruleset lets the owner), and re-run failed jobs. If the commit is on `main`, see the row for a release on the wrong commit. |
| `plan` chose a version whose tag exists on a commit that is not on `main`, and `publish` fails at its tag check on every run | A hand-pushed tag, no release | List the commits the tag names (`git ls-remote origin 'refs/tags/v*'`) and check each is not on `main`: `git fetch origin main` then `git merge-base --is-ancestor <commit> origin/main`, which exits 1 for a commit that is not. Delete such a tag (`git push origin :refs/tags/vX.Y.Z`), then re-run failed jobs (or push a change that ships). Never delete a tag whose commit is on `main` or that a published release uses. |
| A published release is on the wrong commit (the tag does not name the commit the files were built from) | A public release that must not be trusted | Its files are not changed and the release is not edited. Mark it so on the release page and in a new issue, and cut the next version by pushing a change that ships; users verify with `--source-digest` (above), which fails for these files at the tag's commit. |
| `publish` failed at the last call or after it | Possibly the tag and a published release | Re-run failed jobs: the tag stays, files the release lacks are added, the tag is checked to name the commit. |
| `publish` says a file differs | The release holds a file that is not this run's | Never delete or replace assets of a published release. Cut a new version: push a change that ships (the next version counts from the tag). |
| The run was green but released nothing | `plan` printed why | `main` moved on (the newer push releases this change), a later release exists past the commit, or nothing that ships changed. Nothing to do. |
| "Re-run all jobs" after a successful release | The release is complete | It rebuilds the same files; `publish` compares each against the release and keeps them. If a file differs the run fails and nothing is changed. |

Skipped runs: a release run for a commit that is not the tip of `main` is skipped on purpose (an
attestation records the tip, and a build of an older commit must not be published as the newest). The
push that moved `main` releases the change, unless it fails `ci`; then the next green push that ships
code does.

There is no dispatch and no tag trigger: a `v*` tag pushed by hand does not start a release, and
`workflow_dispatch` does not exist. An old run that is re-run with "Re-run all jobs" cannot publish its
old commit, because `plan` finds `main` past it. A re-run of only the failed jobs does not run `plan`
again; the tag checks in `publish` and the attestation check in `verify` are what guard it (the draft
it finds is replaced when it targets another commit). To release by hand there is nothing to do but merge a change that ships.

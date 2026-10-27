# Releasing js-lease-server and js

## Cutting a release

The project owner pushes a version tag; nothing else starts a release.

1. Set the workspace version in `Cargo.toml` (`[workspace.package] version`) and merge it.
2. Tag the merged commit and push the tag: `git tag -a vX.Y.Z -m "vX.Y.Z" && git push origin vX.Y.Z`.

The `release` workflow (`.github/workflows/release.yml`) then:

- fails at once unless the tag is exactly `v` followed by the workspace version;
- builds the assets below with `cargo build --locked --release`, using the toolchain in `rust-toolchain.toml`;
- writes one `SHA256SUMS` over all assets and checks it;
- attests every asset with build provenance (`actions/attest-build-provenance`);
- creates the GitHub release for the tag with the assets and `SHA256SUMS`.

A pull request that changes the workflow file, and a manual run, build the assets and write `SHA256SUMS` but never attest or publish. The attest and publish jobs run only for a tag push, and each holds only the permission it needs (`id-token` and `attestations` write for attest, `contents` write for publish).

## Assets

For version `X.Y.Z`:

| Asset | Contents |
|---|---|
| `js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js-lease-server` binary and `LICENSE` |
| `js-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | the `js` binary and `LICENSE` |
| `js-X.Y.Z-aarch64-apple-darwin.tar.gz` | the `js` binary and `LICENSE` (macOS on Apple silicon, built for macOS 14 or newer) |
| `SHA256SUMS` | SHA-256 of the three archives |

Each archive unpacks to a directory named like the archive without `.tar.gz`.

The Linux binaries are statically linked against musl, so they run on any Linux host of that architecture, including `gcr.io/distroless/cc-debian12`. The TLS stack (rustls with ring) needs a C compiler for ring, which the workflow gets from `musl-tools`; no crate code changes were needed. The server is not built for macOS.

## Downloading and verifying

Needs the GitHub CLI (`gh`), signed in.

```sh
gh release download vX.Y.Z -R komira-ai/jobscheduler -p 'js-lease-server-*-x86_64-unknown-linux-musl.tar.gz' -p SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS      # on macOS: shasum -a 256 -c --ignore-missing SHA256SUMS
gh attestation verify js-lease-server-X.Y.Z-x86_64-unknown-linux-musl.tar.gz -R komira-ai/jobscheduler
```

For the client, use `-p 'js-[0-9]*-x86_64-unknown-linux-musl.tar.gz'` or `-p 'js-[0-9]*-aarch64-apple-darwin.tar.gz'` instead (`js-[0-9]*` keeps the server archive, whose name starts with `js-lease-server-`, out of the match).

The checksum check proves the download is intact; the attestation check proves the file was built from this repository by the `release` workflow. To pin the check to this workflow and tag, add `--signer-workflow komira-ai/jobscheduler/.github/workflows/release.yml --source-ref refs/tags/vX.Y.Z`.

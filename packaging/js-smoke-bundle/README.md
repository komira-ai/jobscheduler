# The smoke bundle

A darwin bundle of one program, `tree/bin/smoke`, that prints one line: the host name, the user and uid it runs as, the macOS version, and its arguments. It uses only `/bin/sh`, `/bin/hostname`, `/usr/bin/id` and `/usr/bin/sw_vers`, which every macOS has, and carries no toolchain. Its purpose is to prove a Mac host with one job: [docs/runbooks/mac-smoke-bundle.md](../../docs/runbooks/mac-smoke-bundle.md) is the operator's page.

- `tree/` is the bundle's one tree, packed as it is committed: names, file bytes, the execute bit and link targets, nothing else.
- `bundle.toml` is the recipe: the repository and tag it is published under, the fields of the `js-bundle pack` that makes it, and the manifest digest that pack gives. The digest is a function of the tree and the recipe (and of the format's version, through `js-bundle`'s), so it is the same wherever the tree is packed.

The release workflow publishes it on every release (`.github/workflows/release.yml`, the jobs `smoke-bundle-pack`, `smoke-bundle-push` and `smoke-bundle-check`; docs/release.md, "The smoke bundle"): it packs the tree with `cargo xtask release smoke-bundle pack` and refuses a digest that is not the recipe's, pushes it with `js-bundle push` to `ghcr.io/komira-ai/js/smoke` under its tag with the workflow's own token, and pulls it back by digest, anonymously, with the agent's registry client and compares every byte with the tree. The same digest under the tag again is left as it is; a different one under the same tag is refused.

Changing the bundle: edit the tree, run `cargo test -p xtask --test smoke_bundle`, which fails and says the digest the tree packs to now; put that digest in `bundle.toml`, raise `tag`, and change the digest and tag in the runbook (the same test holds the runbook to the recipe). A change here is a release, as a change to a shipped crate is.

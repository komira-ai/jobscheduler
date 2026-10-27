# Layers, purity and how other repositories use the crates

This page is the rules for where a crate sits in the workspace and how another repository
(kbf, the first one) depends on the crates by git revision. The checks are Rust tests; none is
a convention that only a reviewer enforces.

## Layers

`layers.toml` lists the workspace's layers from the bottom up and the crates of each. A crate
may depend, through `[dependencies]`, `[build-dependencies]` and their `[target.*]` forms,
only on a crate of its own layer or a lower one. `[dev-dependencies]` are not edges: a test
may reach up to the crate it exercises (`js-conformance` does).

| layer | holds |
|---|---|
| `types` | `js-types`, the vocabulary every other crate shares |
| `pure` | cores that decide: they take time and I/O as inputs |
| `runtime` | state, clocks, storage and containers, used by servers and nodes |
| `server` | what answers hosts and clients |
| `app` | programs, their libraries, and the repository's tools |

`planned` names in a layer reserve a place for a crate that has not arrived. A crate that
arrives is added to `layers.toml` in the same pull request (a workspace crate in no layer
fails the test), and may not be in two layers.

`cargo test -p xtask --test layering` is the check (`crates/xtask/src/layering.rs`). It reads
the manifests, so it needs no network. It also fails when a layer marked `pure` has a crate
whose `src/lib.rs` lacks the `deny` line below, and it fails on a second `js-types` (below).
Its tests copy the real manifests and `layers.toml` into a scratch directory, plant a defect
(an edge up a layer, a missing `deny`, a git copy of `js-types`) and require the check to
name it.

## Purity

A pure crate never reads a clock, hashes a collection with a per-process seed, waits, or opens
a socket, so a seed replays exactly: time enters as an input and ordered collections stand in
for hashed ones. `clippy.toml` lists the disallowed methods and types
(`Instant::now`, `SystemTime::now`, `thread::sleep`, `HashMap`, `HashSet`, `TcpStream`,
`TcpListener`, `UdpSocket`); the workspace allows them (`[workspace.lints.clippy]`) and a pure
crate turns them back on in `src/lib.rs`:

```rust
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
```

The line must be on one line as written here. The lint job runs clippy with `-D warnings`, so
a clock or a `HashMap` in a pure crate fails it. The list is the one kbf uses for its pure
crates, so code that moves keeps passing under the same rules.

## Build configuration

The workspace builds with the toolchain in `rust-toolchain.toml` (1.95.0), edition 2024,
resolver 3, and the lints in the root `Cargo.toml`: `missing_docs` (every public item is
documented), `unexpected_cfgs` with `cfg(coverage)` declared (cargo-llvm-cov sets it, and a
moved crate may compile code out under it), and the clippy purity lints above. These match
kbf's, so a crate does not pass in one repository and fail in the other.

## Moving a crate in from another repository

A crate arrives as one pull request here and then one there:

1. Here: the crate is added as `crates/<name>` (the directory is the package name), history
   preserved, with its tests, in `layers.toml`, and in `coverage-baseline` at `0 0` (or
   `0 -`): the ratchet admits a new crate only at zero uncovered lines and branches, so a
   crate reaches zero before it moves. Its external dependencies are declared once in the
   root `[workspace.dependencies]`, at the version the other repository uses so the two
   `Cargo.lock` files do not diverge, and `deny.toml` covers their licences.
2. There, after this one merges: the other repository bumps its pinned revision, deletes the
   sources and tests it moved, and keeps the crate's name as a shim (`pub use js_x::*`).
   Between the two pull requests the code exists in both places, and nobody edits the
   other repository's copy.

This repository never depends on the other one.

## Depending on the crates by git revision

The other repository names every `js-*` crate once, in its root `[workspace.dependencies]`,
all at the same revision of this repository:

```toml
js-types = { git = "https://github.com/komira-ai/jobscheduler", rev = "<commit>" }
js-sizing = { git = "https://github.com/komira-ai/jobscheduler", rev = "<commit>" }
```

and its members use them as `js-types.workspace = true`. The repository is public, so no
registry and no credentials are involved; a bump is a deliberate pull request there that
changes the one revision and runs that repository's full CI. A crate here that needs a
change in the other repository's behaviour names that bump in its pull request.

Rules the crates keep so a git dependency works:

- A crate's dependencies on other crates here are `path` dependencies inside this
  repository (cargo resolves them from the same checkout), and its external ones come from
  the root `[workspace.dependencies]`.
- There is one `js-types`. Two copies (one by path, one by git, or two revisions) are two
  different crates, so a `LeaseId` made by one is not the `LeaseId` the other asks for. Here
  the layering test requires exactly one `js-types` in `Cargo.lock`, from the workspace, and
  only path or workspace dependencies on it. The other repository applies the same rule to
  its own lock file: one `js-types` entry, with the git source of this repository, and no
  local crate by that name.
- For local work the other repository patches the dependency to a sibling checkout with
  `[patch."https://github.com/komira-ai/jobscheduler"]` in an uncommitted configuration, never
  in a commit.
- A shim has to forward the Cargo features of the crate it re-exports, and `pub use js_x::*`
  does not re-export macros: the other repository checks its dependents with
  `cargo check --workspace --all-targets --all-features`.

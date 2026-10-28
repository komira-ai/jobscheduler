# Layers, purity and how other repositories use the crates

This page is the rules for where a crate sits in the workspace and how another repository
(kbf, the first one) depends on the crates by git revision. The checks are Rust tests; none is
a convention that only a reviewer enforces.

## Layers

`layers.toml` lists the workspace's layers from the bottom up and the crates of each. A crate
may depend, through `[dependencies]`, `[build-dependencies]` and their `[target.*]` forms,
only on a crate of its own layer or a lower one. `[dev-dependencies]` are not edges: a test
may reach up to the crate it exercises (no crate does today).

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
the manifests, so the manifest rules need no network (the transitive walk below runs
`cargo metadata --locked`). It fails on:

- a crate in no layer, or in two;
- a dependency from a lower layer up to a higher one;
- a non-dev dependency (normal, build or `[target.*]`) of a workspace crate that is neither a
  crate `layers.toml` lists nor a registry dependency declared in the root
  `[workspace.dependencies]`: a `kbf-*` crate (by path, git or version) and an unlisted path
  or git dependency both fail here, so this repository cannot depend on the repository its
  crates were lifted out of. An external dependency must be `name.workspace = true`;
- a pure crate that lacks the `deny` below in its library root or any binary root, that
  relaxes the lints anywhere in its `src/` tree (see Purity), or that reaches a forbidden
  external crate through its resolved normal dependencies (see Purity);
- a second `js-types` (below).

`deny.toml` also bans kbf's crates by name in the resolved graph (cargo-deny matches exact
names, so the list is explicit and is refreshed when kbf's crates change; the layering rule
above is by prefix and covers a crate kbf adds later).

Its tests copy the real manifests, sources and `layers.toml` into a scratch directory, plant a
defect (an edge up a layer, a missing `deny`, an `allow`, a `kbf-types` dependency, a git copy
of `js-types`) and require the check to name it.

## Purity

A pure crate never reads a clock, sleeps, hashes a collection with a per-process seed, or opens
a TCP or UDP socket, so a seed replays exactly: time enters as an input and ordered
collections stand in for hashed ones. This is what the lints cover, no more: `clippy.toml`
lists `Instant::now`, `SystemTime::now`, `thread::sleep`, `HashMap`, `HashSet`, `TcpStream`,
`TcpListener` and `UdpSocket`. Other I/O (`std::fs`, `std::process`, `std::env`, Unix
sockets such as `UnixStream`) is **not** covered and is a matter for review. The workspace
allows the lints (`[workspace.lints.clippy]`) and a pure crate turns them back on in its
library root and in every binary root (`src/main.rs`, `src/bin/*`, a `[[bin]]` path):

```rust
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]
```

The attribute must be on one line, listing both lints (`forbid` instead of `deny` is
accepted); a lint name that only a trailing comment mentions does not count. The lint job runs
clippy with `-D warnings`, so a clock or a `HashMap` in a pure crate fails it. The layering
test also refuses, anywhere under a pure crate's `src/`, an attribute other than `deny` or
`forbid` that names `clippy::disallowed_*`, `clippy::all` or `clippy::style` (an `allow`, an
`expect`, a `warn`); a lint name inside a comment that follows another attribute is not
read as one.

A pure crate also reaches no async runtime, network stack or random source, directly or through
any other crate: tokio, tonic, hyper, reqwest, rand, rand_core, rand_chacha, getrandom, mio,
socket2 (the list kbf's layering test keeps for its pure crates). The test walks the resolved
normal dependencies from `cargo metadata`; dev and build dependencies are not linked into the
library and are not walked.

## Build configuration and parity with kbf

The workspace builds with the toolchain in `rust-toolchain.toml` (1.95.0), edition 2024 and
resolver 3. Where it matches kbf and where it does not, so a moved crate is not surprised:

- The same as kbf: toolchain, edition, resolver 3, `unexpected_cfgs` with `cfg(coverage)`
  declared (cargo-llvm-cov sets it, and a moved crate may compile code out under it), and the
  `clippy.toml` lists.
- **Stricter here, deliberately**: `missing_docs = "warn"` in `[workspace.lints.rust]`, and the
  lint job runs clippy with `-D warnings`, so every undocumented public item fails. kbf has no
  `missing_docs`, so a crate that passes there can fail here. A moved crate documents its
  public API, or carries a scoped `#![allow(missing_docs)]` with a comment naming the
  follow-up issue that removes it.
- **Different files**: this repository has a `rustfmt.toml` (`edition = 2024`); kbf has none.
  `cargo fmt` reads the edition from the manifest in both, but the file is a difference to
  keep in mind when formatting moved code.

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

# jobscheduler
A generic job and compute scheduler: capacity providers that acquire, renew and release hosts of a shape, placement, and sizing from request history.

The design is in `docs/design.md` (it arrives with the design pull request). The code is a Cargo workspace of crates under `crates/`, built with the toolchain pinned in `rust-toolchain.toml`. Build with `cargo build --workspace --locked` and test with `cargo test --workspace --locked`; CI also requires `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings` and per-crate line and branch coverage: `crates/js-coverage` compares each crate with its line in `coverage-baseline` (zero uncovered lines and branches to start), so a change must cover the code it adds.

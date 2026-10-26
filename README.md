# jobscheduler
A generic job and compute scheduler: capacity providers that acquire, renew and release hosts of a shape, placement, and sizing from request history.

The design is in `docs/design.md` (it arrives with the design pull request). The code is a Cargo workspace of crates under `crates/`, built with the toolchain pinned in `rust-toolchain.toml`. Build with `cargo build --workspace --locked` and test with `cargo test --workspace --locked`; CI also requires `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings` and per-crate line and branch coverage: `crates/js-coverage` compares each crate with its line in `coverage-baseline` (zero uncovered lines and branches to start), so a change must cover the code it adds.

## Coverage

Every crate must keep zero uncovered lines and branches. The rule is a ratchet, stricter than a fixed percentage: `coverage-baseline` records, per crate, how many lines and branches no test runs, and CI fails when a crate misses more than its line says, or fewer (so the file never keeps slack). When coverage improves, run `cargo run -p js-coverage -- tighten` on the measured file the coverage job uploads (the failing job prints the exact command); `tighten` only lowers counts.

The file is also compared with the base branch's, so editing it to match new uncovered code does not help: a pull request fails when any crate's missed-line or missed-branch count is higher than on the base branch (`js-coverage against-base --base <file> --head coverage-baseline`), and a crate that is new enters at zero. Lowering a count or removing a crate passes.

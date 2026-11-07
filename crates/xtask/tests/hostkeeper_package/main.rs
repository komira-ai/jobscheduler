//! The hostkeeper packages `cargo xtask release hostkeeper-package` writes, read by the code that
//! reads them on a host: hostkeeper's unpacker, manifest loader and placed-path linker, at the
//! commit the workspace pins (docs/release.md). The archive's bytes are read here too: its zstd
//! frame, its tar headers, the manifest as TOML, the unit's assignments and the plist's values.
//! Every platform is built and read on a Linux runner from a stand-in binary; the binary is
//! never run.
//!
//! - `archive`: one zstd frame, the entries and their headers, the binary byte for byte, the same
//!   bytes on every run;
//! - `linux`: the two Linux packages unpacked and loaded, the manifest's keys and bounds, the
//!   unit and the agent's own parser;
//! - `macos`: the Mac package, its plist and its manifest;
//! - `floor`: the oldest hostkeeper a package names;
//! - `install`: a package placed by hostkeeper's linker over fakes, step by step;
//! - `verbs`: the two verbs and each way they refuse (their command line is `tests/hostkeeper_package_cli.rs`).

mod archive;
mod floor;
mod install;
mod linux;
mod macos;
mod support;
mod verbs;

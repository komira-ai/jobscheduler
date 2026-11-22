//! A native Mac run's collected output committed to tierstore's put ingest by the agent,
//! after the run's processes are swept, so the job itself never holds a writable put
//! directory.
//!
//! The put ingest is a plain local directory, the put root, that the tierstore node on the
//! Mac watches. A file's key is its path below the root: the agent commits the object
//! `cas/b/<build>/<nonce>/pack` in the job's put directory `<root>/cas/b/<build>/<nonce>`
//! by three names, in this order:
//!
//! 1. `pack.part`: the bytes are written and synced; the node never takes a name ending in
//!    `.part`.
//! 2. `pack.sha256-<64 hex>.part`: the rename declares the SHA-256 the bytes must have.
//! 3. `pack`: the rename publishes it. The node stores the bytes under the key when they
//!    have the declared digest (it refuses and removes them otherwise), then removes the
//!    file and the empty job directory.
//!
//! What is committed is the run's one output, read from the agent's own copy of it (the
//! runtime collects it into `<scratch>/outputs` before it commits), never from the job's
//! folder: one regular file, committed as it is (the build service reads the key as its
//! pack). Nothing is followed: a link, or anything that is not a regular file, is refused
//! with its path and nothing is published. `pack` is written mode `0640`, so the node's
//! group, which the job's put directory belongs to, can read it whatever the agent's umask.
//!
//! [`target_of`] says whether a directory is a job's put directory: strictly beneath the
//! put root, exactly `cas/b/<build>/<nonce>`, with no link on the way. [`commit`] makes it,
//! hands it to the node ([`commit`]'s `ready`) and commits; it refuses a directory that
//! already holds anything, so a committed key is never written twice. Nothing here reads a
//! clock or a random source: the nonce is the one in the path the build service gave.
//!
//! The module is free of `cfg(target_os)` and uses Unix file calls only, so its tests run
//! on Linux; the macOS runtime (`native/put.rs`) gives it the put root, the node's user and
//! the run's collected output.

mod commit;
mod target;

pub use commit::{Committed, PutTarget, commit};
pub use target::{Refusal, target_of};

#[cfg(test)]
mod commit_tests;
#[cfg(test)]
mod target_tests;

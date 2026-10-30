//! The in-process registry of js-oci's tests, `crates/js-oci/tests/support/mod.rs`, for the
//! tests of a crate that pulls through js-oci (feature `oci-registry`).
//!
//! The source file is js-oci's own, included here and not copied, so js-oci's tests and a
//! caller's tests pull from the same registry and a behaviour added there is available to
//! both. [`Fixture`] is the registry, [`Artifact`] what it serves: a container image
//! ([`Artifact::image`]) or a bundle, with their manifests and blobs.

// The file was written for a test binary: it allows what a test uses only some of.
#![allow(missing_docs)]

#[path = "../../js-oci/tests/support/mod.rs"]
mod registry;

pub use registry::*;

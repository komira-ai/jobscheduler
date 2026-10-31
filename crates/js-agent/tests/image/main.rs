//! The crun runtime's image handling: where the layers of an image come from and what
//! the image config says about running it. The registry is the in-process one js-oci's
//! tests use (`js-testkit`, feature `oci-registry`); pins, redirects, the allow-list, Bearer
//! tokens and the manifest and blob checks are js-oci's and are tested in its `pull.rs`,
//! not repeated here: these tests are about what the adapter makes of them.

#![cfg(target_os = "linux")]

mod config;
mod pull;
mod source;

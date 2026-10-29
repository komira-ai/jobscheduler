//! The registry client of this repository: an OCI artifact named by manifest digest is
//! pulled from its registry with every byte checked against a digest on the way in.
//!
//! What is verified, and when:
//! - the manifest's bytes hash to the digest the artifact is pinned by, before anything
//!   in them is believed; an image index (or Docker manifest list) is refused, since an
//!   index digest names a different artifact on each platform; the manifest must be of
//!   the [`Kind`] the caller asks for (media types, `artifactType`, layer count and
//!   sizes);
//! - the config blob and every layer blob are hashed while they arrive and compared with
//!   the manifest's digest and size before the caller's [`BlobSink`] is told they are
//!   whole;
//! - the artifact's own platform, as its config states it, is the [`Kind`]'s: a bundle
//!   is `darwin/arm64` and its config must say so; an image is a platform the caller
//!   chooses, and its config may leave the platform out.
//!
//! Where the client may go ([`Policy`]):
//! - a pin may name only a registry and namespace on the allow-list ([`Allow`]);
//! - every connection goes to an address the client resolved and checked itself, so a
//!   name that resolves to loopback, link-local, private, shared (`100.64.0.0/10`),
//!   unique-local or IPv4-mapped forms of those is refused, whatever it is called. A
//!   registry on a private address is reached only if its entry says [`Allow::private`],
//!   and only for that host;
//! - a token endpoint (`realm`) must be the registry's host or another listed host;
//! - https only. Redirects are followed at most five times, to https targets only, with
//!   no `Authorization`.
//!
//! The client decodes nothing: layer bytes go to the caller as they were stored.
//!
//! ```ignore
//! let client = Client::new(Policy::new(vec![Allow::parse("ghcr.io/komira-ai/")?]));
//! let repo = client.repository(Location::parse("ghcr.io/komira-ai/bundle")?)?;
//! let kind = Kind::bundle();
//! let manifest = repo.fetch_manifest(&pin, &kind).await?;
//! let config = repo.fetch_config(&manifest, &kind).await?;
//! for layer in &manifest.layers {
//!     repo.fetch_blob(layer, &mut sink).await?;
//! }
//! ```

mod client;
mod digest;
mod error;
mod http;
mod location;
mod manifest;
mod policy;

pub use client::{BlobSink, Client, Repository};
pub use digest::Digest;
pub use error::Error;
pub use http::Timeouts;
pub use location::{Authority, Location};
pub use manifest::{Descriptor, Kind, Limits, Manifest, Platform, media};
pub use policy::{AddressClass, Allow, Policy, classify};

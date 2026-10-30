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
//!   name that resolves to loopback, link-local, a cloud metadata address
//!   (`169.254.169.254`, `fd00:ec2::254`, Azure's `168.63.129.16`), private, shared
//!   (`100.64.0.0/10`), unique-local, site-local, benchmarking (`198.18.0.0/15`),
//!   `192.0.0.0/24`, multicast or reserved, or an IPv6 form that carries one of those
//!   IPv4 addresses (mapped, compatible, NAT64, 6to4, Teredo), is refused, whatever it
//!   is called ([`classify`]). A registry on a private network is reached only if its
//!   entry says [`Allow::private`], and only for that host; that covers the private,
//!   shared and unique-local classes and never loopback, link-local, metadata,
//!   unspecified or multicast addresses. A registry on this machine needs
//!   [`Allow::loopback`], for development and tests;
//! - a token endpoint (`realm`) must be the registry's host, another listed host, or a
//!   host added with [`Policy::allow_realm`]. Docker Hub's tokens come from
//!   `auth.docker.io`, which is none of the first two for `docker.io` pins, so a client
//!   that pulls from Docker Hub needs `allow_realm("auth.docker.io")` (an example is on
//!   [`Policy::allow_realm`]);
//! - https only, and only port 443 or the registry's own port for any request,
//!   redirect or token endpoint, so a registry cannot send the client to scan other
//!   ports of a public host. Redirects are followed at most five times, to https
//!   targets only, with no `Authorization`, and a `Location` is resolved as RFC 3986
//!   says;
//! - one exception to https: an entry marked [`Allow::plain_http_loopback`] whose host is
//!   the IP literal `127.0.0.1` or `[::1]` is pulled over plain http, with no TLS. Its
//!   requests, redirects and token realms may go only to that exact scheme, host and
//!   port; the flag is ignored on a name (`localhost` included) and on every other
//!   address, and does not touch other entries.
//!
//! Where the client does *not* look:
//! - `HTTP_PROXY`, `HTTPS_PROXY` and `NO_PROXY` are deliberately not honoured: the
//!   client connects to the addresses it resolved and checked, so a proxy the
//!   environment names would take that check from it. A host that can only reach its
//!   registry through a proxy needs a transport of its own, not this client.
//! - The system trust store is the platform's, except that `SSL_CERT_FILE` and
//!   `SSL_CERT_DIR` in the environment replace it (see [`Client::new`]); a caller that
//!   must not depend on its environment uses [`Client::with_roots`], and
//!   [`Client::roots`] reports how many roots loaded and how many did not.
//!
//! What an error says: URLs have no query, fragment or userinfo (a redirect to a CDN
//! carries a signature in its query), and text a registry supplied is cut to 128 bytes
//! and escaped, so an error is safe to log.
//!
//! Time: every wait has a limit ([`Timeouts`]), and a whole manifest or blob has a
//! wall-clock limit on top of the per-wait ones, so a registry that drips bytes cannot
//! hold a pull open.
//!
//! Concurrency: a [`Repository`] takes `&self` everywhere and the client does not limit
//! how many fetches are in flight; the caller bounds them (four at a time, say).
//!
//! The client decodes nothing: layer bytes go to the caller as they were stored.
//!
//! Where the bytes come from is a [`BundleSource`]: a [`Repository`] of a registry, or a
//! [`Layout`], an OCI image-layout directory on disk (a release asset unpacked for a host
//! with no registry, or a test). Both run the same checks (the manifest's digest, kind,
//! limits and index refusal, and each blob's length and digest), so what one accepts the
//! other accepts, and the bytes are the same. A layout is read without following a
//! symlink below its root and refuses a version it does not know.
//!
//! ```ignore
//! let client = Client::new(Policy::new(vec![Allow::parse("ghcr.io/komira-ai/")?]));
//! let repo = client.repository(Location::parse("ghcr.io/komira-ai/bundle")?)?;
//! let kind = Kind::bundle();
//! let manifest = repo.fetch_manifest(&pin, &kind).await?; // with `BundleSource` in scope
//! let config = repo.fetch_config(&manifest, &kind).await?;
//! for layer in manifest.layers() {
//!     repo.fetch_blob(layer, &mut sink).await?;
//! }
//! ```

mod client;
mod digest;
mod error;
mod http;
mod layout;
mod location;
mod manifest;
mod policy;
mod source;
mod text;
mod verify;

pub use client::{BlobSink, Client, Repository};
pub use digest::Digest;
pub use error::Error;
pub use http::{RootSummary, Timeouts};
pub use layout::{Layout, LayoutFault};
pub use location::{Authority, Location};
pub use manifest::{Descriptor, Kind, Limits, Manifest, Platform, media};
pub use policy::{AddressClass, Allow, Policy, classify};
pub use source::BundleSource;

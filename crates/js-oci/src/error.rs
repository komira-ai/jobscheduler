//! What can go wrong pulling an artifact.

use std::net::IpAddr;

use crate::policy::AddressClass;

/// Why an artifact could not be pulled.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A repository name that is not `<registry>/<path>`, or a registry or namespace that
    /// is not well formed.
    #[error("{0:?} is not a well formed registry reference")]
    Reference(String),
    /// A digest that is not `sha256:` and 64 lowercase hex digits.
    #[error("{0:?} is not sha256: and 64 lowercase hex digits")]
    Digest(String),
    /// A registry or namespace that is not on the allow-list.
    #[error("{registry}/{repository} is not on the registry allow-list")]
    NotAllowed {
        /// The registry as the pin names it.
        registry: String,
        /// The repository path.
        repository: String,
    },
    /// A URL that is not https.
    #[error("{0} is not https")]
    Insecure(String),
    /// A connection to an address the allow-list does not permit.
    #[error("{host} resolves to {addr}, a {class} address")]
    Address {
        /// The host as the URL named it.
        host: String,
        /// The address it resolved to.
        addr: IpAddr,
        /// Why the address is refused.
        class: AddressClass,
    },
    /// A token endpoint on a host that is neither the registry's nor an allowed one.
    #[error("the token realm {0} is not the registry's host or an allowed host")]
    Realm(String),
    /// Connecting, the TLS handshake, the request or a body failed.
    #[error("{what}: {why}")]
    Transport {
        /// The URL, or the thing being done.
        what: String,
        /// What went wrong.
        why: String,
    },
    /// A request was answered with a status that is not success.
    #[error("{url}: the registry answered {status}")]
    Status {
        /// The URL asked.
        url: String,
        /// The status.
        status: u16,
    },
    /// The registry's authentication challenge could not be answered.
    #[error("registry authentication: {0}")]
    Auth(String),
    /// A response is larger than its limit.
    #[error("{what} is larger than {limit} bytes")]
    TooLarge {
        /// What was too large.
        what: String,
        /// The limit, in bytes or in items.
        limit: u64,
    },
    /// The manifest's bytes do not hash to the digest it is pinned by.
    #[error("the manifest hashes to {actual}, not the pinned {pinned}")]
    ManifestDigest {
        /// The pin.
        pinned: String,
        /// What the bytes hash to.
        actual: String,
    },
    /// The digest names an image index or manifest list.
    #[error("{0} names an image index; pin the manifest of one platform")]
    Index(String),
    /// A manifest of another artifact type than the one asked for.
    #[error("the manifest has artifact type {have:?}, not {want}")]
    ArtifactType {
        /// The type asked for.
        want: String,
        /// The type the manifest states.
        have: Option<String>,
    },
    /// A manifest or config feature this client does not take.
    #[error("{0}")]
    Unsupported(String),
    /// A manifest, config or token that is not what its type says.
    #[error("{0}")]
    Malformed(String),
    /// A blob's bytes do not hash to its digest.
    #[error("the blob {digest} hashes to {actual}")]
    BlobDigest {
        /// The digest the manifest names.
        digest: String,
        /// What the bytes hash to.
        actual: String,
    },
    /// A blob's length is not the manifest's.
    #[error("the blob {digest} is {got} bytes, the manifest says {declared}")]
    BlobSize {
        /// The digest the manifest names.
        digest: String,
        /// The length the manifest states.
        declared: u64,
        /// The length received (so far, if it passed the declared one).
        got: u64,
    },
    /// The artifact is for another platform than the one asked for.
    #[error("the artifact is for {have}, not {want}")]
    Platform {
        /// The platform asked for, `os/architecture`.
        want: String,
        /// What the config states.
        have: String,
    },
    /// The machine's architecture has no platform name here.
    #[error("the architecture {0} has no image platform name")]
    Arch(String),
    /// The sink a blob was written to failed.
    #[error("writing the blob: {0}")]
    Sink(#[source] std::io::Error),
}

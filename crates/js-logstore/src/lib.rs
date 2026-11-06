//! The log object store: where a job's live logs are kept, and the client that reads and
//! writes them (`docs/design/live-logs.md`).
//!
//! - [`layout`] names the objects: a job's prefix `P = prefix/job_id/` under the host's prefix
//!   `prefix`, its chunks
//!   `P/stdout/<offset20>` and `P/stderr/<offset20>`, its gap markers (`<offset20>.gap`, body
//!   `{"length":N}`) and its finals `P/stdout.log` and `P/stderr.log`. The agent, the scheduler
//!   and the reader all build names here, so they agree.
//! - [`sigv4`] signs a request with AWS Signature Version 4 (header form, hashed payload,
//!   session tokens), for the `s3` service or any other (`sts`).
//! - [`Store`] is the S3 client: `put` (one request, at most [`MAX_PUT_BYTES`], with object
//!   metadata), `delete` (a missing key is success), `get_range` and `list` (ListObjectsV2),
//!   path-style or virtual-host-style, over a [`Transport`]. [`UreqTransport`] is the production
//!   transport; the `fake` feature's [`fake::FakeStore`] is an in-memory bucket that checks
//!   signatures, session policies and prefix grants.
//! - [`presigned`] is the upload credential of a store with no STS: a [`PrefixGrant`] scopes a
//!   derived key to one prefix until an absolute expiry, and [`PrefixSigner`] presigns puts and
//!   deletes with it, refusing a key outside the prefix or a time past the expiry before any
//!   request exists.
//!
//! **Credentials.** A [`Credential`] is an access key id, a secret, an optional session token and
//! an expiry. The client never sends a request with an expired one ([`StoreError::Expired`]);
//! nothing here mints one.
//!
//! **Errors** ([`StoreError`]) are told apart by transport kind, HTTP status and S3 error code,
//! never by message text: `Unavailable` (transport failure, timeout, 5xx, 429, `SlowDown`),
//! `Denied` (403 other than an expiry), `Expired` (`ExpiredToken`, `RequestExpired`,
//! `InvalidToken`, MinIO's `InvalidAccessKeyId` and `InvalidTokenId`, or the credential's own
//! expiry), `NotFound`, `TooLarge`, `Invalid`, `OutOfRange` and `Malformed`.
//!
//! **Secrets.** No error, `Debug` output or log line of this crate holds a secret access key,
//! a session token, a signature or a URL's query.
//!
//! **What the crate never does.** It never retries (each call is one request; the callers own
//! retry and backoff), never reads an environment variable or a file, and starts no thread or
//! async runtime.

pub mod client;
pub mod clock;
pub mod credential;
pub mod error;
#[cfg(feature = "fake")]
pub mod fake;
pub mod http;
pub mod layout;
pub mod list;
pub mod presigned;
pub mod sigv4;
pub mod ureq_transport;

pub use client::{MAX_GET_BYTES, MAX_LIST_KEYS, Metadata, Object, Store};
pub use clock::{Clock, ManualClock, SystemClock};
pub use credential::{Credential, Endpoint, EndpointError};
pub use error::{StoreError, Unavailable};
pub use http::{HttpRequest, HttpResponse, Method, Transport, TransportError};
pub use layout::{
    ChunkName, Gap, JobPrefix, LayoutError, MAX_CHUNK_BYTES, MAX_PUT_BYTES, OFFSET_WIDTH, Stream,
};
pub use list::{Entry, Page};
pub use presigned::{GrantError, PrefixGrant, PrefixSigner};
pub use ureq_transport::{MAX_RESPONSE_BYTES, UreqTransport};
